//! Wire round-trip, multiplexing, event, and fault tests.
//!
//! The server-side tests drive [`serve_connection`] over a `tokio::io::duplex`
//! pipe with a frame-level client, so no socket is involved. One end-to-end test
//! runs a real [`WireNamespace`] over a `UnixListener` in a tempdir.

use std::net::Ipv4Addr;
use std::path::{Path as StdPath, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures::future::{BoxFuture, FutureExt};
use omnifs_core::path::Path;
use omnifs_engine::{
    Attrs, DirCursor, DirEntry, DirPage, EntryKind, EventStream, LookupAnswer, Namespace, NsError,
    NsEvent, ReadAnswer, ReadStyle, StabilityClass,
};
use tokio::io::{AsyncWriteExt, DuplexStream};
use tokio::sync::broadcast;

use crate::frame::{
    Frame, KIND_EVENT, KIND_REQUEST, KIND_RESPONSE, MAX_FRAME, read_frame, write_frame,
};
use crate::{
    AttachTarget, FrontendIdentity, FrontendKind, Handshake, ListenerTarget, PROTOCOL, VfsServer,
    WireError, WireNamespace, WireRequest, WireResponse, serve_connection,
};

const VALID_TOKEN: &str = "0123456789abcdef0123456789abcdef";
const EVENT_CAPACITY: usize = 1024;

fn path(value: &str) -> Path {
    Path::parse(value).expect("valid test path")
}

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
    emit_on_read: AtomicUsize,
}

impl StubNamespace {
    fn new() -> Arc<Self> {
        let (events, _) = broadcast::channel(1);
        Arc::new(Self {
            events,
            emit_on_read: AtomicUsize::new(0),
        })
    }
}

fn file_attrs(size: u64) -> Attrs {
    Attrs {
        kind: EntryKind::File,
        dev: 0,
        ino: 0,
        size,
        blocks: size.div_ceil(512),
        mode: 0o444,
        nlink: 1,
        accessed: None,
        modified: None,
        created: None,
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
        _parent: Path,
        name: &'a str,
    ) -> BoxFuture<'a, Result<LookupAnswer, NsError>> {
        let name = name.to_string();
        async move {
            Ok(LookupAnswer {
                path: path(if name == "message" {
                    "/test/message"
                } else {
                    "/test/child"
                }),
                attrs: file_attrs(13),
            })
        }
        .boxed()
    }

    fn getattr(&self, path: Path) -> BoxFuture<'_, Result<Attrs, NsError>> {
        async move { Ok(file_attrs(path.as_str().len() as u64 / 2)) }.boxed()
    }

    fn getattr_exact(&self, path: Path) -> BoxFuture<'_, Result<Attrs, NsError>> {
        async move { Ok(file_attrs(path.as_str().len() as u64)) }.boxed()
    }

    fn readdir(
        &self,
        _path: Path,
        _cursor: DirCursor,
        _budget: usize,
    ) -> BoxFuture<'_, Result<DirPage, NsError>> {
        async move {
            Ok(DirPage {
                entries: vec![DirEntry {
                    name: "child".to_string(),
                    path: path("/test/child"),
                    attrs: file_attrs(1),
                }],
                next: Some(DirCursor::Buffered {
                    entries: Vec::new(),
                    then: None,
                    offline: false,
                }),
            })
        }
        .boxed()
    }

    fn read(
        &self,
        read_path: Path,
        offset: u64,
        _len: u32,
    ) -> BoxFuture<'_, Result<ReadAnswer, NsError>> {
        async move {
            if self.emit_on_read.swap(0, Ordering::SeqCst) != 0 {
                let _ = self.events.send(NsEvent::InvalidateSubtree {
                    path: read_path.clone(),
                });
            }
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

    fn readlink(&self, path: Path) -> BoxFuture<'_, Result<PathBuf, NsError>> {
        async move {
            if path.as_str() == "/test/offline" {
                Err(NsError::OfflineMiss)
            } else {
                Err(NsError::Invalid)
            }
        }
        .boxed()
    }

    fn subscribe(&self) -> EventStream {
        EventStream::from_broadcast(self.events.subscribe())
    }
}

// ---------------------------------------------------------------------------
// Frame-level client helpers over a duplex
// ---------------------------------------------------------------------------

/// Perform the client side of the handshake, returning success or the rejection
/// reason. The wire handshake carries no daemon instance identity.
async fn client_handshake_with_token(
    io: &mut DuplexStream,
    protocol: u32,
    token: Option<String>,
) -> Result<(), String> {
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
        Handshake::Welcome { .. } => Ok(()),
        Handshake::Rejected { reason } => Err(reason),
        Handshake::Hello { .. } => panic!("server sent a hello"),
    }
}

/// Perform the client side of the handshake with no token.
async fn client_handshake(io: &mut DuplexStream, protocol: u32) -> Result<(), WireError> {
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

fn start_local_server(namespace: Arc<dyn Namespace>, path: PathBuf) -> Arc<VfsServer> {
    let server = VfsServer::new(namespace);
    server.serve_local(path).unwrap();
    server
}

fn start_tcp_server(
    namespace: Arc<dyn Namespace>,
    token: &str,
) -> (Arc<VfsServer>, ListenerTarget) {
    let server = VfsServer::new(namespace);
    let target = server
        .ensure_tcp(Ipv4Addr::LOCALHOST, 0, Some(token.to_string()))
        .unwrap();
    (server, target)
}

fn start_vsock_server(
    namespace: Arc<dyn Namespace>,
    path: &StdPath,
    token: &str,
) -> Arc<VfsServer> {
    let server = VfsServer::new(namespace);
    server.ensure_vsock(path, Some(token.to_string())).unwrap();
    server
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
    serve_over_duplex_with(namespace, expected_token)
}

fn serve_over_duplex_with(
    namespace: Arc<dyn Namespace>,
    expected_token: Option<&'static str>,
) -> (DuplexStream, tokio::task::JoinHandle<Result<(), WireError>>) {
    let (client_io, server_io) = tokio::io::duplex(1024 * 1024);
    let handle = tokio::spawn(serve_connection(namespace, server_io, expected_token));
    (client_io, handle)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn round_trips_every_request_variant() {
    let stub = StubNamespace::new();
    let (mut io, _server) = serve_over_duplex(stub);
    client_handshake(&mut io, PROTOCOL).await.unwrap();

    send_request(
        &mut io,
        1,
        &WireRequest::Lookup {
            parent: path("/"),
            name: "message".to_string(),
        },
    )
    .await;
    let (id, resp) = recv_response(&mut io).await;
    assert_eq!(id, 1);
    match resp {
        WireResponse::Lookup(Ok(answer)) => assert_eq!(answer.path, path("/test/message")),
        other => panic!("unexpected {other:?}"),
    }

    send_request(
        &mut io,
        2,
        &WireRequest::Getattr {
            path: path("/test/five"),
        },
    )
    .await;
    match recv_response(&mut io).await {
        (2, WireResponse::Getattr(Ok(attrs))) => assert_eq!(attrs.size, 5),
        other => panic!("unexpected {other:?}"),
    }

    send_request(
        &mut io,
        3,
        &WireRequest::GetattrExact {
            path: path("/test/five"),
        },
    )
    .await;
    match recv_response(&mut io).await {
        (3, WireResponse::GetattrExact(Ok(attrs))) => assert_eq!(attrs.size, 10),
        other => panic!("unexpected {other:?}"),
    }

    send_request(
        &mut io,
        4,
        &WireRequest::Readdir {
            path: path("/"),
            cursor: DirCursor::start(),
            budget: 0,
        },
    )
    .await;
    match recv_response(&mut io).await {
        (4, WireResponse::Readdir(Ok(page))) => {
            assert_eq!(page.entries.len(), 1);
            assert_eq!(page.entries[0].name, "child");
            assert!(matches!(
                page.next,
                Some(DirCursor::Buffered { offline: false, .. })
            ));
        },
        other => panic!("unexpected {other:?}"),
    }

    send_request(
        &mut io,
        5,
        &WireRequest::Read {
            path: path("/test/one"),
            offset: 0,
            len: 8,
        },
    )
    .await;
    match recv_response(&mut io).await {
        (5, WireResponse::Read(Ok(answer))) => assert!(answer.eof),
        other => panic!("unexpected {other:?}"),
    }

    send_request(
        &mut io,
        6,
        &WireRequest::Readlink {
            path: path("/test/one"),
        },
    )
    .await;
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
                path: path("/test/one"),
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
// One fixture proves the initial snapshot plus ordered push delivery over the
// same connection; splitting it would duplicate the protocol setup.
#[allow(clippy::too_many_lines)]
async fn server_pushes_events() {
    let stub = StubNamespace::new();
    let events = stub.events.clone();
    let (mut io, server) = serve_over_duplex(Arc::clone(&stub) as Arc<dyn Namespace>);
    client_handshake(&mut io, PROTOCOL).await.unwrap();

    // The event forwarder subscribes right after the handshake; wait for it, then
    // push one event and read it off the wire.
    while events.receiver_count() == 0 {
        tokio::task::yield_now().await;
    }
    let newest = NsEvent::AttrsChanged {
        path: path("/test/events"),
        attrs: file_attrs(9),
    };
    events
        .send(NsEvent::AttrsChanged {
            path: path("/test/old"),
            attrs: file_attrs(8),
        })
        .unwrap();
    events.send(newest.clone()).unwrap();

    let first = read_frame(&mut io).await.unwrap().expect("lag event frame");
    let second = read_frame(&mut io)
        .await
        .unwrap()
        .expect("newest event frame");
    assert_eq!(first.kind, KIND_EVENT);
    assert_eq!(second.kind, KIND_EVENT);
    assert_eq!(
        postcard::from_bytes::<NsEvent>(&first.body).unwrap(),
        NsEvent::InvalidateSubtree { path: Path::root() }
    );
    assert_eq!(
        postcard::from_bytes::<NsEvent>(&second.body).unwrap(),
        newest
    );

    // An operation-caused event is enqueued synchronously by the namespace
    // before its response becomes available to the server writer.
    stub.emit_on_read.store(1, Ordering::SeqCst);
    send_request(
        &mut io,
        7,
        &WireRequest::Read {
            path: path("/test/events"),
            offset: 0,
            len: 1,
        },
    )
    .await;
    let event = read_frame(&mut io).await.unwrap().expect("operation event");
    let response = read_frame(&mut io)
        .await
        .unwrap()
        .expect("operation response");
    assert_eq!(event.kind, KIND_EVENT);
    assert_eq!(
        postcard::from_bytes::<NsEvent>(&event.body).unwrap(),
        NsEvent::InvalidateSubtree {
            path: path("/test/events")
        }
    );
    assert_eq!(response.kind, KIND_RESPONSE);
    assert_eq!(response.request_id, 7);
    match postcard::from_bytes::<WireResponse>(&response.body).unwrap() {
        WireResponse::Read(Ok(answer)) => assert_eq!(answer.bytes, 0_u64.to_le_bytes()),
        other => panic!("unexpected operation response {other:?}"),
    }

    // A sustained event stream must not starve a response or prevent the
    // connection task from observing client shutdown. The sender capacity is
    // deliberately exceeded so the server's bounded event snapshot also
    // exercises its lag-to-root-invalidation path.
    for index in 0..(EVENT_CAPACITY + 64) {
        events
            .send(NsEvent::AttrsChanged {
                path: path(&format!("/test/flood/{index}")),
                attrs: file_attrs(index as u64),
            })
            .unwrap();
    }
    send_request(
        &mut io,
        8,
        &WireRequest::Read {
            path: path("/test/events"),
            offset: 1,
            len: 1,
        },
    )
    .await;
    let mut response_seen = false;
    for _ in 0..(EVENT_CAPACITY + 128) {
        let frame = tokio::time::timeout(Duration::from_secs(2), read_frame(&mut io))
            .await
            .expect("event flood must not starve the response")
            .unwrap()
            .expect("flood connection remains live");
        if frame.kind == KIND_RESPONSE && frame.request_id == 8 {
            response_seen = true;
            break;
        }
    }
    assert!(
        response_seen,
        "request 8 response must survive the event flood"
    );
    drop(io);
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("server must shut down after client disconnect")
        .expect("server task must not panic")
        .expect("server must report clean disconnect");
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
    // The client offers the immediately previous strict protocol version.
    let hello = postcard::to_allocvec(&Handshake::Hello {
        protocol: PROTOCOL - 1,
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
            assert_eq!(theirs, PROTOCOL - 1);
        },
        other => panic!("expected VersionMismatch, got {other:?}"),
    }
}

#[tokio::test]
async fn tcp_style_token_is_accepted_when_it_matches() {
    let stub = StubNamespace::new();
    let (mut io, _server) = serve_over_duplex_with_token(stub, Some("right-token"));

    client_handshake_with_token(&mut io, PROTOCOL, Some("right-token".to_string()))
        .await
        .unwrap();
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

    client_handshake_with_token(&mut io, PROTOCOL, Some("whatever".to_string()))
        .await
        .unwrap();
}

#[tokio::test]
async fn server_side_nserror_propagates() {
    let stub = StubNamespace::new();
    let (mut io, _server) = serve_over_duplex(stub);
    client_handshake(&mut io, PROTOCOL).await.unwrap();

    send_request(
        &mut io,
        1,
        &WireRequest::Readlink {
            path: path("/test/one"),
        },
    )
    .await;
    match recv_response(&mut io).await {
        (1, WireResponse::Readlink(Err(NsError::Invalid))) => {},
        other => panic!("expected Invalid, got {other:?}"),
    }
    send_request(
        &mut io,
        2,
        &WireRequest::Readlink {
            path: path("/test/offline"),
        },
    )
    .await;
    match recv_response(&mut io).await {
        (2, WireResponse::Readlink(Err(NsError::OfflineMiss))) => {},
        other => panic!("expected OfflineMiss, got {other:?}"),
    }
}

#[tokio::test]
async fn unix_listener_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("ns.sock");
    let stub = StubNamespace::new();
    let server = start_local_server(stub, socket.clone());

    let namespace = WireNamespace::attach(
        AttachTarget::Unix(socket),
        test_identity(),
        tokio::runtime::Handle::current(),
    )
    .await
    .expect("attach");
    let answer = namespace.lookup(path("/"), "message").await.unwrap();
    assert_eq!(answer.path, path("/test/message"));

    let attrs = namespace.getattr(path("/test/five")).await.unwrap();
    assert_eq!(attrs.size, 5);

    let read = namespace.read(path("/test/one"), 0, 8).await.unwrap();
    assert!(read.eof);

    let err = namespace.readlink(path("/test/one")).await.unwrap_err();
    assert_eq!(err, NsError::Invalid);
    server.shutdown().await;
}

#[tokio::test]
async fn startup_gate_holds_listener_until_ready_publication() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("ns.sock");
    let server = VfsServer::new(StubNamespace::new());
    let _control_gate = server.begin_startup();
    server.serve_local(socket.clone()).unwrap();

    let mut stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
    let hello = postcard::to_allocvec(&Handshake::Hello {
        protocol: PROTOCOL,
        token: None,
        frontend: test_identity(),
    })
    .unwrap();
    write_frame(&mut stream, &Frame::new(0, KIND_REQUEST, hello))
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(25), read_frame(&mut stream))
            .await
            .is_err(),
        "the listener must not serve before startup publication"
    );

    server.mark_ready();
    let welcome = read_frame(&mut stream)
        .await
        .unwrap()
        .expect("welcome after startup publication");
    assert!(matches!(
        postcard::from_bytes::<Handshake>(&welcome.body).unwrap(),
        Handshake::Welcome { protocol } if protocol == PROTOCOL
    ));
    server.shutdown().await;
}

/// The Docker Desktop path end to end: a real TCP loopback listener, a real
/// [`WireNamespace`] dialing it with the matching attach token.
#[tokio::test]
async fn tcp_listener_end_to_end() {
    let stub = StubNamespace::new();
    let (server, ListenerTarget::Tcp { addr, token }) = start_tcp_server(stub, VALID_TOKEN) else {
        panic!("TCP server returned a non-TCP target")
    };

    let namespace = WireNamespace::attach(
        AttachTarget::Tcp {
            addr: addr.to_string(),
            token,
        },
        test_identity(),
        tokio::runtime::Handle::current(),
    )
    .await
    .expect("attach");
    let answer = namespace.lookup(path("/"), "message").await.unwrap();
    assert_eq!(answer.path, path("/test/message"));
    server.shutdown().await;
}

#[tokio::test]
async fn tcp_listener_rejects_wrong_token() {
    let stub = StubNamespace::new();
    let (server, ListenerTarget::Tcp { addr, .. }) = start_tcp_server(stub, VALID_TOKEN) else {
        panic!("TCP server returned a non-TCP target")
    };

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
    server.shutdown().await;
}

/// The libkrun vsock-proxy path's host-side shape: a real token-authenticated
/// UDS listener, so a connecting peer must present it exactly like the TCP
/// listener does. Driven with the
/// raw frame helpers (not `WireNamespace::attach`/`AttachTarget::Unix`, which
/// by design never sends a token) since production reaches this socket through
/// libkrun's vsock proxy, not a bare Unix dial.
#[tokio::test]
async fn unix_listener_with_token_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("ns.sock");
    let stub = StubNamespace::new();
    let server = start_vsock_server(stub.clone(), &socket, VALID_TOKEN);

    let mut stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
    let hello = postcard::to_allocvec(&Handshake::Hello {
        protocol: PROTOCOL,
        token: Some(VALID_TOKEN.to_string()),
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
        Handshake::Welcome { protocol } => assert_eq!(protocol, PROTOCOL),
        other => panic!("expected Welcome, got {other:?}"),
    }
    drop(stream);
    server.shutdown().await;
    assert!(!socket.exists(), "dynamic UDS must be removed on shutdown");

    let rebound = start_vsock_server(stub, &socket, VALID_TOKEN);
    assert!(socket.exists(), "dynamic UDS must be bindable again");
    rebound.shutdown().await;
    assert!(!socket.exists(), "rebound UDS must be cleaned too");
}

#[tokio::test]
async fn unix_listener_with_token_rejects_wrong_token() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("ns.sock");
    let stub = StubNamespace::new();
    let server = start_vsock_server(stub, &socket, VALID_TOKEN);

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
    server.shutdown().await;
}

#[tokio::test]
async fn invalid_vsock_token_is_rejected_before_binding() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("invalid.sock");
    let server = VfsServer::new(StubNamespace::new());

    let error = server
        .ensure_vsock(&socket, Some("not-a-valid-token".to_string()))
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(!socket.exists());

    server.shutdown().await;
}

#[tokio::test]
async fn removing_dynamic_listener_recovers_readiness() {
    let dir = tempfile::tempdir().unwrap();
    let server = VfsServer::new(StubNamespace::new());
    server.serve_local(dir.path().join("local.sock")).unwrap();
    let (target, newly_bound) = server
        .ensure_tcp_with_status(Ipv4Addr::LOCALHOST, 0, Some(VALID_TOKEN.to_string()))
        .unwrap();
    assert!(newly_bound);

    server.mark_ready();
    assert!(server.ready());
    assert!(server.remove_listener(&target));
    assert!(server.ready());

    server.shutdown().await;
}

#[tokio::test]
async fn unix_listener_never_follows_an_existing_symlink() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target.sock");
    let socket = dir.path().join("listener.sock");
    let target_listener = std::os::unix::net::UnixListener::bind(&target).unwrap();
    symlink(&target, &socket).unwrap();
    let server = VfsServer::new(StubNamespace::new());

    server.serve_local(socket.clone()).unwrap();

    assert!(
        !std::fs::symlink_metadata(&socket)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(target.exists(), "the symlink target must remain untouched");
    server.shutdown().await;
    assert!(
        target.exists(),
        "shutdown must remove only the owned listener"
    );
    drop(target_listener);
}

/// A disconnected wire namespace publishes one root invalidation, fails an
/// outage request promptly, drops that request before the replacement
/// handshake, and accepts fresh uncached requests after reconnect.
#[tokio::test]
// Keep disconnect invalidation, queued-request rejection, and reconnect in one
// lifecycle fixture so their ordering remains observable.
#[allow(clippy::too_many_lines)]
async fn tcp_disconnect_invalidates_root_and_queued_path_request_reconnects() {
    // Exercise the complete dial-plus-Welcome deadline without waiting thirty
    // real seconds. The server accepts Hello but deliberately never answers it.
    let stalled_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let stalled_addr = stalled_listener.local_addr().unwrap();
    let stalled_rt = tokio::runtime::Handle::current();
    let stalled_attach = tokio::spawn(WireNamespace::attach(
        AttachTarget::Tcp {
            addr: stalled_addr.to_string(),
            token: VALID_TOKEN.to_string(),
        },
        test_identity(),
        stalled_rt,
    ));
    let (mut stalled_stream, _) = stalled_listener.accept().await.unwrap();
    let stalled_hello = read_frame(&mut stalled_stream)
        .await
        .unwrap()
        .expect("stalled hello frame");
    assert!(matches!(
        postcard::from_bytes::<Handshake>(&stalled_hello.body).unwrap(),
        Handshake::Hello { .. }
    ));
    // Keep the socket setup on real time so the immediate Hello cannot be
    // skipped by Tokio's paused-clock auto-advance. Pause only after the
    // handshake has reached its deliberately stalled Welcome read.
    tokio::time::pause();
    let stalled_result = tokio::time::timeout(Duration::from_secs(31), stalled_attach)
        .await
        .expect("stalled Welcome must hit the advertised deadline")
        .expect("stalled attach task must not panic");
    assert!(matches!(
        stalled_result,
        Err(WireError::ConnectTimeout { .. })
    ));
    drop(stalled_stream);
    tokio::time::resume();

    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = std_listener.local_addr().unwrap();
    std_listener.set_nonblocking(true).unwrap();
    let listener = tokio::net::TcpListener::from_std(std_listener).unwrap();
    let rt = tokio::runtime::Handle::current();
    let token = VALID_TOKEN.to_string();

    let attach_target = AttachTarget::Tcp {
        addr: addr.to_string(),
        token: token.clone(),
    };
    let attach_task = rt.spawn(WireNamespace::attach(
        attach_target,
        test_identity(),
        rt.clone(),
    ));

    // Establish the initial instance and check the presented token. Keep this
    // stream alive until the namespace subscriber is installed, otherwise EOF
    // can publish the root invalidation before the test can observe it.
    let (mut stream_a, _) = listener.accept().await.unwrap();
    let hello_frame = read_frame(&mut stream_a)
        .await
        .unwrap()
        .expect("hello frame");
    let Handshake::Hello {
        token: presented, ..
    } = postcard::from_bytes(&hello_frame.body).unwrap()
    else {
        panic!("expected a hello frame");
    };
    assert_eq!(presented.as_deref(), Some(token.as_str()));
    let welcome = postcard::to_allocvec(&Handshake::Welcome { protocol: PROTOCOL }).unwrap();
    write_frame(&mut stream_a, &Frame::new(0, KIND_RESPONSE, welcome))
        .await
        .unwrap();

    let ns = attach_task.await.unwrap().expect("initial attach");
    let mut events = ns.subscribe();
    drop(stream_a);
    let stable = path("/stable");

    // The manager observes the disconnect before dialing the replacement.
    let root = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("disconnect root invalidation")
        .expect("event stream remains live");
    assert_eq!(root, NsEvent::InvalidateSubtree { path: Path::root() });

    // This request belongs to the outage epoch and must fail promptly instead
    // of waiting behind the reconnect handshake.
    let queued = tokio::spawn({
        let ns = Arc::clone(&ns);
        let stable = stable.clone();
        async move { ns.getattr(stable).await }
    });

    // Accept the replacement and keep its stream under direct test control.
    let (mut stream_b, _) = listener.accept().await.unwrap();
    let hello_frame = read_frame(&mut stream_b)
        .await
        .unwrap()
        .expect("second hello frame");
    let Handshake::Hello { .. } = postcard::from_bytes(&hello_frame.body).unwrap() else {
        panic!("expected a hello frame");
    };
    let welcome = postcard::to_allocvec(&Handshake::Welcome { protocol: PROTOCOL }).unwrap();
    write_frame(&mut stream_b, &Frame::new(0, KIND_RESPONSE, welcome))
        .await
        .unwrap();

    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), queued)
            .await
            .expect("outage request must fail promptly")
            .expect("queued task must not panic")
            .expect_err("queued request must fail on the old connection"),
        NsError::Network
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), read_frame(&mut stream_b))
            .await
            .is_err(),
        "the queued outage request must not replay on the replacement"
    );

    for _ in 0..2 {
        let (call, frame) = loop {
            let call = tokio::spawn({
                let ns = Arc::clone(&ns);
                let stable = stable.clone();
                async move { ns.getattr(stable).await }
            });
            match tokio::time::timeout(Duration::from_millis(250), read_frame(&mut stream_b)).await
            {
                Ok(Ok(Some(frame))) => break (call, frame),
                Ok(Ok(None)) => panic!("replacement connection closed"),
                Ok(Err(error)) => panic!("replacement read failed: {error}"),
                Err(_) => {
                    assert_eq!(call.await.unwrap().unwrap_err(), NsError::Network);
                },
            }
        };
        let request: WireRequest = postcard::from_bytes(&frame.body).unwrap();
        assert!(matches!(request, WireRequest::Getattr { path } if path == stable));
        let body = postcard::to_allocvec(&WireResponse::Getattr(Ok(file_attrs(7)))).unwrap();
        write_frame(
            &mut stream_b,
            &Frame::new(frame.request_id, KIND_RESPONSE, body),
        )
        .await
        .unwrap();
        assert_eq!(call.await.unwrap().unwrap().size, 7);
    }
    drop(stream_b);
}

mod trace_propagation {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    use omnifs_api::events::InspectorEvent;
    use omnifs_core::path::Path;
    use omnifs_engine::Namespace;
    use tokio::runtime::Handle;
    use tracing_subscriber::prelude::*;

    use crate::{
        AttachTarget, FrontendIdentity, FrontendKind, ListenerTarget, VfsServer, WireNamespace,
    };

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
    /// the one Path `TreeNamespace` resolved for this call.
    #[tokio::test(flavor = "current_thread")]
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
        let _subscriber =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(sink.layer()));

        let harness = omnifs_itest::make_runtime();
        let tree_ns = Arc::clone(&harness.namespace);

        let server = VfsServer::new(tree_ns);
        let target = server
            .ensure_tcp(
                "127.0.0.1".parse().unwrap(),
                0,
                Some(super::VALID_TOKEN.to_string()),
            )
            .unwrap();
        let ListenerTarget::Tcp { addr, token } = target else {
            panic!("trace server returned a non-TCP target")
        };

        let client = WireNamespace::attach(
            AttachTarget::Tcp {
                addr: addr.to_string(),
                token,
            },
            FrontendIdentity {
                kind: FrontendKind::Fuse,
                mount_point: PathBuf::from("/mnt/trace-test"),
            },
            Handle::current(),
        )
        .await
        .expect("attach");

        // Mirrors what a frontend does to serve `cat /test/hello/message`:
        // resolve through the mount root and two lookups, then read the whole
        // (fresh, uncached) file, so
        // the read triggers a real provider callout underneath.
        let mount = client.lookup(Path::root(), "test").await.unwrap();
        let hello = client.lookup(mount.path, "hello").await.unwrap();
        let message = client.lookup(hello.path, "message").await.unwrap();
        let read = client.read(message.path, 0, 4096).await.unwrap();
        assert_eq!(read.bytes, b"Hello, world!");

        server.shutdown().await;

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
