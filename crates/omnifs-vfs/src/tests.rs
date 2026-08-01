//! Wire round-trip, multiplexing, event, and fault tests.
//!
//! The server-side tests drive [`serve_connection`] over a `tokio::io::duplex`
//! pipe with a frame-level client, so no socket is involved. One end-to-end test
//! runs a real [`WireNamespace`] over a `UnixListener` in a tempdir.

use std::net::Ipv4Addr;
use std::num::NonZeroU16;
use std::path::{Path as StdPath, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use crate::{
    Attrs, DirCursor, DirEntry, DirPage, EntryKind, EventStream, LookupAnswer, Namespace,
    NamespaceEpoch, NamespaceEvent, NamespaceEventHub, NamespaceLease, NamespaceSubscription,
    NsError, NsEvent, ReadAnswer, ReadStyle, ServingNamespace, Stability,
};
use futures::future::{BoxFuture, FutureExt};
use omnifs_core::path::Path;
use tokio::io::{AsyncWriteExt, DuplexStream};
use tokio::sync::broadcast;

use crate::frame::{
    Frame, KIND_EVENT, KIND_REQUEST, KIND_RESPONSE, MAX_FRAME, read_frame, write_frame,
};
use crate::{
    AttachTarget, Endpoint, Handshake, PROTOCOL, Session, VfsServer, WireError, WireNamespace,
    WireReply, WireRequest, WireResponse, serve_connection,
};

const EVENT_CAPACITY: usize = 1024;

fn path(value: &str) -> Path {
    Path::parse(value).expect("valid test path")
}

/// A canned identity for tests that don't care about the specific value, only
/// that a `Hello` carries one.
fn test_filesystem() -> omnifs_core::ResourceName {
    "test".parse().unwrap()
}

fn test_identity() -> omnifs_core::FilesystemSpec {
    let (protocol, runtime, location) = if cfg!(target_os = "linux") {
        (
            omnifs_core::FilesystemProtocol::Fuse,
            omnifs_core::FilesystemRuntime::Host,
            PathBuf::from("/mnt/test"),
        )
    } else {
        (
            omnifs_core::FilesystemProtocol::Nfs,
            omnifs_core::FilesystemRuntime::Host,
            PathBuf::from("/mnt/test"),
        )
    };
    omnifs_core::FilesystemSpec::new(protocol, runtime, location, None, None).unwrap()
}

fn test_runtime_instance() -> String {
    "0123456789abcdef0123456789abcdef".to_owned()
}

fn test_epoch() -> NamespaceEpoch {
    NamespaceEpoch::initial([0x42; 16])
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
        stability: Stability::Stable,
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
            let path = path(if name == "message" {
                "/test/message"
            } else {
                "/test/child"
            });
            let attrs = file_attrs(13);
            Ok(LookupAnswer::found(path, attrs))
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

struct StaticServingNamespace {
    namespace: Arc<dyn Namespace>,
    events: Arc<NamespaceEventHub>,
    cancellation: tokio::sync::watch::Sender<bool>,
}

impl StaticServingNamespace {
    fn new(namespace: Arc<dyn Namespace>) -> Arc<Self> {
        let epoch = test_epoch();
        let events = NamespaceEventHub::new(epoch, EVENT_CAPACITY);
        let mut source = namespace.subscribe();
        let event_sink = Arc::clone(&events);
        tokio::spawn(async move {
            while let Some(event) = source.recv().await {
                event_sink.publish_if_current(epoch, event);
            }
        });
        Arc::new(Self {
            namespace,
            events,
            cancellation: tokio::sync::watch::channel(false).0,
        })
    }
}

impl ServingNamespace for StaticServingNamespace {
    fn acquire(&self) -> Result<NamespaceLease, NsError> {
        Ok(NamespaceLease::new(
            self.current_epoch(),
            Arc::clone(&self.namespace),
            (),
            self.cancellation.subscribe(),
        ))
    }

    fn subscribe(&self) -> NamespaceSubscription {
        self.events.subscribe()
    }

    fn current_epoch(&self) -> NamespaceEpoch {
        self.events.current_epoch()
    }
}

// ---------------------------------------------------------------------------
// Frame-level client helpers over a duplex
// ---------------------------------------------------------------------------

/// Perform the client side of the handshake, returning success or the
/// rejection reason. The wire handshake carries no daemon instance identity.
async fn client_handshake(io: &mut DuplexStream, protocol: u32) -> Result<(), WireError> {
    let hello = postcard::to_allocvec(&Handshake::Hello {
        protocol,
        filesystem: test_filesystem(),
        spec: test_identity(),
        runtime_instance: test_runtime_instance(),
    })
    .unwrap();
    write_frame(io, &Frame::new(0, KIND_REQUEST, hello)).await?;
    let welcome = read_frame(io).await?.expect("welcome frame");
    match postcard::from_bytes::<Handshake>(&welcome.body).unwrap() {
        Handshake::Welcome { .. } => Ok(()),
        Handshake::Rejected { reason } => Err(WireError::Rejected(reason)),
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
    let reply: WireReply = postcard::from_bytes(&frame.body).unwrap();
    assert_eq!(reply.epoch, test_epoch());
    (frame.request_id, reply.response)
}

fn start_local_server(namespace: Arc<dyn Namespace>, path: &StdPath) -> Arc<VfsServer> {
    let server = VfsServer::new(StaticServingNamespace::new(namespace));
    server.serve_unix(path).unwrap();
    server
}

fn start_tcp_server(namespace: Arc<dyn Namespace>) -> (Arc<VfsServer>, Endpoint) {
    let probe = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = NonZeroU16::new(probe.local_addr().unwrap().port()).unwrap();
    drop(probe);
    let server = VfsServer::new(StaticServingNamespace::new(namespace));
    let target = server.serve_tcp(Ipv4Addr::LOCALHOST, port).unwrap();
    (server, target)
}

/// Spawn a server over the server half of a fresh duplex; return the client
/// half and the server's join handle.
fn serve_over_duplex(
    namespace: Arc<dyn Namespace>,
) -> (DuplexStream, tokio::task::JoinHandle<Result<(), WireError>>) {
    let (client_io, server_io) = tokio::io::duplex(1024 * 1024);
    let handle = tokio::spawn(serve_connection(
        StaticServingNamespace::new(namespace),
        server_io,
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
        postcard::from_bytes::<NamespaceEvent>(&first.body)
            .unwrap()
            .into_event(),
        NsEvent::reset()
    );
    assert_eq!(
        postcard::from_bytes::<NamespaceEvent>(&second.body)
            .unwrap()
            .into_event(),
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
        postcard::from_bytes::<NamespaceEvent>(&event.body)
            .unwrap()
            .into_event(),
        NsEvent::InvalidateSubtree {
            path: path("/test/events")
        }
    );
    assert_eq!(response.kind, KIND_RESPONSE);
    assert_eq!(response.request_id, 7);
    match postcard::from_bytes::<WireReply>(&response.body)
        .unwrap()
        .response
    {
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
        filesystem: test_filesystem(),
        spec: test_identity(),
        runtime_instance: test_runtime_instance(),
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
async fn malformed_runtime_instance_is_rejected_before_session_admission() {
    let stub = StubNamespace::new();
    let (mut io, server) = serve_over_duplex(stub);
    let hello = postcard::to_allocvec(&Handshake::Hello {
        protocol: PROTOCOL,
        filesystem: test_filesystem(),
        spec: test_identity(),
        runtime_instance: "not-an-exact-runtime-instance".to_owned(),
    })
    .unwrap();
    write_frame(&mut io, &Frame::new(0, KIND_REQUEST, hello))
        .await
        .unwrap();

    match server.await.unwrap() {
        Err(WireError::Protocol(detail)) => {
            assert!(detail.contains("32 lowercase hexadecimal"), "{detail}");
        },
        other => panic!("expected strict runtime identity rejection, got {other:?}"),
    }
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
    let server = start_local_server(stub, &socket);

    let namespace = WireNamespace::attach(
        AttachTarget::Unix(socket),
        test_filesystem(),
        test_identity(),
        test_runtime_instance(),
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
    let server = VfsServer::new(StaticServingNamespace::new(StubNamespace::new()));
    let _control_gate = server.begin_startup();
    server.serve_unix(&socket).unwrap();

    let mut stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
    let hello = postcard::to_allocvec(&Handshake::Hello {
        protocol: PROTOCOL,
        filesystem: test_filesystem(),
        spec: test_identity(),
        runtime_instance: test_runtime_instance(),
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
        Handshake::Welcome { protocol, .. } if protocol == PROTOCOL
    ));
    server.shutdown().await;
}

/// The Docker Desktop path end to end: a real TCP loopback listener, a real
/// [`WireNamespace`] dialing it.
#[tokio::test]
async fn tcp_listener_end_to_end() {
    let stub = StubNamespace::new();
    let (server, Endpoint::Tcp { addr }) = start_tcp_server(stub) else {
        panic!("TCP server returned a non-TCP target")
    };

    let namespace = WireNamespace::attach(
        AttachTarget::Tcp {
            addr: addr.to_string(),
        },
        test_filesystem(),
        test_identity(),
        test_runtime_instance(),
        tokio::runtime::Handle::current(),
    )
    .await
    .expect("attach");
    let answer = namespace.lookup(path("/"), "message").await.unwrap();
    assert_eq!(answer.path, path("/test/message"));
    server.shutdown().await;
}

#[tokio::test]
async fn one_name_allows_reconnect_overlap_but_rejects_conflicting_resolved_fields() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("ns.sock");
    let server = start_local_server(StubNamespace::new(), &socket);
    let first = WireNamespace::attach(
        AttachTarget::Unix(socket.clone()),
        test_filesystem(),
        test_identity(),
        test_runtime_instance(),
        tokio::runtime::Handle::current(),
    )
    .await
    .unwrap();
    let overlap = WireNamespace::attach(
        AttachTarget::Unix(socket.clone()),
        test_filesystem(),
        test_identity(),
        test_runtime_instance(),
        tokio::runtime::Handle::current(),
    )
    .await
    .unwrap();
    assert_eq!(server.sessions().len(), 1);

    let conflicting = omnifs_core::FilesystemSpec::new(
        omnifs_core::FilesystemProtocol::Fuse,
        omnifs_core::FilesystemRuntime::Docker,
        PathBuf::from(omnifs_core::FILESYSTEM_GUEST_LOCATION),
        None,
        None,
    )
    .unwrap();
    let error = WireNamespace::attach(
        AttachTarget::Unix(socket),
        test_filesystem(),
        conflicting,
        test_runtime_instance(),
        tokio::runtime::Handle::current(),
    )
    .await
    .err()
    .expect("one filesystem name must not carry conflicting exact specs");
    assert!(
        matches!(error, WireError::Rejected(reason) if reason.contains("different exact spec"))
    );

    drop(overlap);
    drop(first);
    server.shutdown().await;
}

#[tokio::test]
async fn unapproved_runtime_instance_replacement_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("ns.sock");
    let server = start_local_server(StubNamespace::new(), &socket);
    let first = WireNamespace::attach(
        AttachTarget::Unix(socket.clone()),
        test_filesystem(),
        test_identity(),
        test_runtime_instance(),
        tokio::runtime::Handle::current(),
    )
    .await
    .unwrap();
    let second = WireNamespace::attach(
        AttachTarget::Unix(socket),
        test_filesystem(),
        test_identity(),
        "fedcba9876543210fedcba9876543210".to_owned(),
        tokio::runtime::Handle::current(),
    )
    .await
    .err()
    .expect("a different runtime instance cannot replace a live session");
    assert!(matches!(second, WireError::Rejected(_)));
    assert_eq!(server.sessions().len(), 1);
    drop(first);
    server.shutdown().await;
}

#[tokio::test]
async fn supervisor_approved_replacement_fences_the_old_session() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("ns.sock");
    let server = start_local_server(StubNamespace::new(), &socket);
    let old_instance = test_runtime_instance();
    let old = Session {
        filesystem: test_filesystem(),
        spec: test_identity(),
        runtime_instance: old_instance.clone(),
    };
    let replacement = Session {
        filesystem: test_filesystem(),
        spec: test_identity(),
        runtime_instance: "fedcba9876543210fedcba9876543210".to_owned(),
    };
    let first = WireNamespace::attach(
        AttachTarget::Unix(socket.clone()),
        old.filesystem.clone(),
        old.spec.clone(),
        old_instance,
        tokio::runtime::Handle::current(),
    )
    .await
    .unwrap();
    assert!(server.wait_for_session(&old, Duration::from_secs(1)).await);
    server
        .begin_session_replacement(&old, &replacement)
        .expect("supervisor explicitly approves the replacement");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    while !server.sessions().is_empty() {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::task::yield_now().await;
    }
    let replacement_connection = WireNamespace::attach(
        AttachTarget::Unix(socket),
        replacement.filesystem.clone(),
        replacement.spec.clone(),
        replacement.runtime_instance.clone(),
        tokio::runtime::Handle::current(),
    )
    .await
    .expect("only the supervisor-approved instance replaces the old one");
    assert!(
        server
            .wait_for_session(&replacement, Duration::from_secs(1))
            .await
    );
    drop(replacement_connection);
    drop(first);
    server.shutdown().await;
}

#[tokio::test]
async fn exact_session_stop_fences_reconnect_until_runtime_cleanup_finishes() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("ns.sock");
    let server = start_local_server(StubNamespace::new(), &socket);
    let expected = Session {
        filesystem: test_filesystem(),
        spec: test_identity(),
        runtime_instance: test_runtime_instance(),
    };
    let (teardown_tx, mut teardown_rx) = tokio::sync::mpsc::channel(1);
    let namespace = WireNamespace::attach_with_teardown(
        AttachTarget::Unix(socket.clone()),
        expected.filesystem.clone(),
        expected.spec.clone(),
        expected.runtime_instance.clone(),
        tokio::runtime::Handle::current(),
        teardown_tx,
    )
    .await
    .unwrap();
    assert!(
        server
            .wait_for_session(&expected, Duration::from_secs(1))
            .await
    );

    server.begin_session_stop(&expected).unwrap();
    let request = tokio::time::timeout(Duration::from_secs(1), teardown_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(request.reason(), crate::TeardownReason::ServerStop);
    let rejected = WireNamespace::attach(
        AttachTarget::Unix(socket.clone()),
        expected.filesystem.clone(),
        expected.spec.clone(),
        expected.runtime_instance.clone(),
        tokio::runtime::Handle::current(),
    )
    .await
    .err()
    .expect("an exact stop fence must reject reconnects");
    assert!(matches!(rejected, WireError::Rejected(_)));

    request.complete(crate::TeardownOutcome::Busy);
    server.close_stopped_session(&expected).unwrap();
    assert!(
        server
            .drain_sessions(Duration::from_secs(1))
            .await
            .is_empty()
    );
    server.finish_session_stop(&expected).unwrap();
    let replacement = WireNamespace::attach(
        AttachTarget::Unix(socket),
        expected.filesystem.clone(),
        expected.spec.clone(),
        expected.runtime_instance.clone(),
        tokio::runtime::Handle::current(),
    )
    .await
    .expect("runtime cleanup releases the exact stop fence");

    drop(replacement);
    drop(namespace);
    server.shutdown().await;
}

#[tokio::test]
async fn exact_session_stop_fences_reconnect_when_the_session_is_already_gone() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("ns.sock");
    let server = start_local_server(StubNamespace::new(), &socket);
    let expected = Session {
        filesystem: test_filesystem(),
        spec: test_identity(),
        runtime_instance: test_runtime_instance(),
    };

    server.begin_session_stop(&expected).unwrap();
    let rejected = WireNamespace::attach(
        AttachTarget::Unix(socket.clone()),
        expected.filesystem.clone(),
        expected.spec.clone(),
        expected.runtime_instance.clone(),
        tokio::runtime::Handle::current(),
    )
    .await
    .err()
    .expect("runtime teardown must fence a reconnect after its old session has gone");
    assert!(matches!(rejected, WireError::Rejected(_)));

    server.finish_session_stop(&expected).unwrap();
    let replacement = WireNamespace::attach(
        AttachTarget::Unix(socket),
        expected.filesystem.clone(),
        expected.spec.clone(),
        expected.runtime_instance.clone(),
        tokio::runtime::Handle::current(),
    )
    .await
    .expect("runtime cleanup releases the exact stop fence");

    drop(replacement);
    server.shutdown().await;
}

#[tokio::test]
async fn server_stop_reaches_client_and_drain_waits_for_detach() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("ns.sock");
    let server = start_local_server(StubNamespace::new(), &socket);
    let (teardown_tx, mut teardown_rx) = tokio::sync::mpsc::channel(1);
    let namespace = WireNamespace::attach_with_teardown(
        AttachTarget::Unix(socket),
        test_filesystem(),
        test_identity(),
        test_runtime_instance(),
        tokio::runtime::Handle::current(),
        teardown_tx,
    )
    .await
    .unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    while server.sessions().is_empty() {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::task::yield_now().await;
    }
    server.stop_sessions();
    let request = tokio::time::timeout(Duration::from_secs(1), teardown_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(request.reason(), crate::TeardownReason::ServerStop);
    request.complete(crate::TeardownOutcome::Stopped);
    assert!(
        server
            .drain_sessions(Duration::from_secs(1))
            .await
            .is_empty()
    );
    drop(namespace);
    server.shutdown().await;
}

#[tokio::test]
async fn busy_client_remains_a_named_drain_straggler_and_new_admission_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("ns.sock");
    let server = start_local_server(StubNamespace::new(), &socket);
    let (teardown_tx, mut teardown_rx) = tokio::sync::mpsc::channel(1);
    let namespace = WireNamespace::attach_with_teardown(
        AttachTarget::Unix(socket.clone()),
        test_filesystem(),
        test_identity(),
        test_runtime_instance(),
        tokio::runtime::Handle::current(),
        teardown_tx,
    )
    .await
    .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    while server.sessions().is_empty() {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::task::yield_now().await;
    }
    server.stop_sessions();
    let request = teardown_rx.recv().await.unwrap();
    request.complete(crate::TeardownOutcome::Busy);
    let stragglers = server.drain_sessions(Duration::from_millis(20)).await;
    assert_eq!(stragglers.len(), 1);
    assert_eq!(stragglers[0].filesystem, test_filesystem());
    assert_eq!(stragglers[0].spec, test_identity());

    let rejected = WireNamespace::attach(
        AttachTarget::Unix(socket),
        test_filesystem(),
        test_identity(),
        test_runtime_instance(),
        tokio::runtime::Handle::current(),
    )
    .await
    .err()
    .expect("draining server must reject a new filesystem");
    assert!(matches!(rejected, WireError::Rejected(_)));
    drop(namespace);
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
    let server = VfsServer::new(StaticServingNamespace::new(StubNamespace::new()));

    server.serve_unix(&socket).unwrap();

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

#[tokio::test]
async fn newer_reply_resets_before_acceptance_and_stale_reply_retries() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let attach = tokio::spawn(WireNamespace::attach(
        AttachTarget::Tcp {
            addr: addr.to_string(),
        },
        test_filesystem(),
        test_identity(),
        test_runtime_instance(),
        tokio::runtime::Handle::current(),
    ));
    let (mut stream, _) = listener.accept().await.unwrap();
    let hello = read_frame(&mut stream).await.unwrap().expect("hello");
    assert!(matches!(
        postcard::from_bytes::<Handshake>(&hello.body).unwrap(),
        Handshake::Hello { .. }
    ));
    let initial = test_epoch();
    let newer = initial.next().unwrap();
    let welcome = postcard::to_allocvec(&Handshake::Welcome {
        protocol: PROTOCOL,
        epoch: initial,
    })
    .unwrap();
    write_frame(&mut stream, &Frame::new(0, KIND_RESPONSE, welcome))
        .await
        .unwrap();
    let namespace = attach.await.unwrap().unwrap();
    let mut events = namespace.subscribe();

    let first_call = tokio::spawn({
        let namespace = Arc::clone(&namespace);
        async move { namespace.getattr(path("/newer")).await }
    });
    let first_request = read_frame(&mut stream)
        .await
        .unwrap()
        .expect("first request");
    let newer_reply = postcard::to_allocvec(&WireReply {
        epoch: newer,
        response: WireResponse::Getattr(Ok(file_attrs(7))),
    })
    .unwrap();
    write_frame(
        &mut stream,
        &Frame::new(first_request.request_id, KIND_RESPONSE, newer_reply),
    )
    .await
    .unwrap();
    assert_eq!(events.recv().await, Some(NsEvent::reset()));
    assert_eq!(first_call.await.unwrap().unwrap().size, 7);

    let retrying_call = tokio::spawn({
        let namespace = Arc::clone(&namespace);
        async move { namespace.getattr(path("/stale")).await }
    });
    let stale_request = read_frame(&mut stream)
        .await
        .unwrap()
        .expect("stale request");
    let stale_reply = postcard::to_allocvec(&WireReply {
        epoch: initial,
        response: WireResponse::Getattr(Ok(file_attrs(8))),
    })
    .unwrap();
    write_frame(
        &mut stream,
        &Frame::new(stale_request.request_id, KIND_RESPONSE, stale_reply),
    )
    .await
    .unwrap();
    let retry = read_frame(&mut stream)
        .await
        .unwrap()
        .expect("retried request");
    assert_ne!(retry.request_id, stale_request.request_id);
    let current_reply = postcard::to_allocvec(&WireReply {
        epoch: newer,
        response: WireResponse::Getattr(Ok(file_attrs(9))),
    })
    .unwrap();
    write_frame(
        &mut stream,
        &Frame::new(retry.request_id, KIND_RESPONSE, current_reply),
    )
    .await
    .unwrap();
    assert_eq!(retrying_call.await.unwrap().unwrap().size, 9);
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
        },
        test_filesystem(),
        test_identity(),
        test_runtime_instance(),
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

    let attach_target = AttachTarget::Tcp {
        addr: addr.to_string(),
    };
    let attach_task = rt.spawn(WireNamespace::attach(
        attach_target,
        test_filesystem(),
        test_identity(),
        test_runtime_instance(),
        rt.clone(),
    ));

    // Establish the initial instance. Keep this stream alive until the
    // namespace subscriber is installed, otherwise EOF can publish the root
    // invalidation before the test can observe it.
    let (mut stream_a, _) = listener.accept().await.unwrap();
    let hello_frame = read_frame(&mut stream_a)
        .await
        .unwrap()
        .expect("hello frame");
    assert!(matches!(
        postcard::from_bytes::<Handshake>(&hello_frame.body).unwrap(),
        Handshake::Hello { .. }
    ));
    let welcome = postcard::to_allocvec(&Handshake::Welcome {
        protocol: PROTOCOL,
        epoch: test_epoch(),
    })
    .unwrap();
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
    assert_eq!(root, NsEvent::reset());

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
    let welcome = postcard::to_allocvec(&Handshake::Welcome {
        protocol: PROTOCOL,
        epoch: test_epoch(),
    })
    .unwrap();
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
        let body = postcard::to_allocvec(&WireReply {
            epoch: test_epoch(),
            response: WireResponse::Getattr(Ok(file_attrs(7))),
        })
        .unwrap();
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
