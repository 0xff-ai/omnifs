//! Kernel-free tracer for the engine `Namespace` surface (runtime redesign
//! phase 2, step 1): drive `TreeNamespace` against the in-tree
//! `test_provider.wasm` with NO fuser, NO mount, NO container.
//!
//! Reuses the omnifs-itest provider-loading harness (`RuntimeHarness` via
//! `make_runtime`), keeps an `Arc<Engine>` clone so the test can fire provider
//! effects directly, and builds a single-mount `TreeNamespace` over the same
//! engine. This proves the narrow frontend-facing surface (opaque node ids,
//! policied attrs, ranged-handle reuse, paging, and the invalidation event
//! stream) before either kernel adapter is ported onto it.
//!
//! Precondition: `just providers build` has produced
//! `target/wasm32-wasip2/release/test_provider.wasm`.

#![cfg(not(target_os = "wasi"))]
// Test docs reference protocol acronyms (FUSE, NFS) and type names as prose.
#![allow(clippy::doc_markdown)]

use std::sync::Arc;
use std::time::Duration;

use omnifs_engine::{
    DirCursor, Engine, Namespace, NodeId, NsEntryKind, NsError, NsEvent, TreeNamespace,
};
use omnifs_itest::{RuntimeHarness, make_engine, make_runtime};
use omnifs_wit::provider::types::{Effects, Invalidation, PathOrPrefix};
use tempfile::TempDir;
use tokio::runtime::Handle;

/// Owns the harness temp dirs that must outlive the engine, the `TreeNamespace`
/// under test, and a second `Arc<Engine>` clone so the test can apply provider
/// effects without going through the namespace surface (which hides them).
struct TestNs {
    ns: Arc<TreeNamespace>,
    runtime: Arc<Engine>,
    _clone_dir: TempDir,
    _cache_dir: TempDir,
    _config_dir: TempDir,
}

fn test_ns() -> TestNs {
    let engine = make_engine();
    let RuntimeHarness {
        clone_dir,
        cache_dir,
        config_dir,
        runtime,
        ..
    } = make_runtime(&engine);
    let runtime = Arc::new(runtime);
    let ns = TreeNamespace::single("test".to_string(), Arc::clone(&runtime), Handle::current());
    TestNs {
        ns,
        runtime,
        _clone_dir: clone_dir,
        _cache_dir: cache_dir,
        _config_dir: config_dir,
    }
}

/// `Invalidation::Listing(Path)` lands in the runtime's invalidated-paths queue,
/// which `Tree::drain_invalidations` (and thus the namespace) consumes.
fn path_invalidation(path: &str) -> Effects {
    Effects {
        canonical: Vec::new(),
        fs: Vec::new(),
        invalidations: vec![Invalidation::Listing(PathOrPrefix::Path(path.to_string()))],
    }
}

// --- root enumeration + descent ---------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn root_enumerates_and_descends_to_message() {
    let t = test_ns();
    let ns = &t.ns;

    // Root lookup finds the `hello` directory the provider projects.
    let hello = ns
        .lookup(NodeId::ROOT, "hello")
        .await
        .expect("lookup /hello");
    assert_eq!(hello.kind, NsEntryKind::Directory);

    // Root readdir enumerates the mount's root children.
    let page = ns
        .readdir(NodeId::ROOT, DirCursor::start(), 0)
        .await
        .expect("readdir root");
    let names: Vec<&str> = page.entries.iter().map(|e| e.name.as_str()).collect();
    assert!(
        names.contains(&"hello"),
        "root readdir lists hello: {names:?}"
    );

    // Descend to /hello/message, a whole-file leaf.
    let message = ns
        .lookup(hello.node, "message")
        .await
        .expect("lookup /hello/message");
    assert_eq!(message.kind, NsEntryKind::File);

    // Before any read, the whole-file leaf reports the unknown-size sentinel.
    let attrs = ns.getattr(message.node).await.expect("getattr message");
    assert_eq!(attrs.size, 1, "unknown-length file reports the 1 sentinel");
}

// --- reads -------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn full_read_slices_by_offset_and_learns_size() {
    let t = test_ns();
    let ns = &t.ns;

    let hello = ns.lookup(NodeId::ROOT, "hello").await.unwrap();
    let message = ns.lookup(hello.node, "message").await.unwrap();

    // "Hello, world!" sliced at offset 2, length 4 => "llo,".
    let answer = ns.read(message.node, 2, 4).await.expect("read slice");
    assert_eq!(answer.bytes, b"llo,");
    assert!(!answer.eof);

    // A whole read learns the exact size and reports EOF at the end.
    let whole = ns.read(message.node, 0, 64).await.expect("read whole");
    assert_eq!(whole.bytes, b"Hello, world!");
    assert!(whole.eof);
    assert_eq!(whole.attrs.size, 13, "the read promotes the exact size");

    // The learned size writes back: a later getattr reports it without a re-read.
    let attrs = ns.getattr(message.node).await.expect("getattr after read");
    assert_eq!(
        attrs.size, 13,
        "learned size survives the placeholder refresh"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn ranged_read_reuses_one_handle_and_reports_eof() {
    let t = test_ns();
    let ns = &t.ns;

    let hello = ns.lookup(NodeId::ROOT, "hello").await.unwrap();
    let ranged = ns.lookup(hello.node, "ranged").await.unwrap();

    // A ranged route opens a provider handle: mid-file chunk "cdef" at offset 2.
    let first = ns.read(ranged.node, 2, 4).await.expect("first ranged read");
    assert_eq!(first.bytes, b"cdef");
    assert!(!first.eof);

    // A second read of the same node reuses the cached handle: still one open.
    let second = ns
        .read(ranged.node, 0, 3)
        .await
        .expect("second ranged read");
    assert_eq!(second.bytes, b"abc");
    assert_eq!(
        ns.ranged_open_count(),
        1,
        "the ranged handle is reused, not reopened"
    );

    // An at-EOF read returns empty + eof (the file is 26 bytes).
    let eof = ns.read(ranged.node, 26, 8).await.expect("eof read");
    assert!(eof.bytes.is_empty());
    assert!(eof.eof);
    assert_eq!(ns.ranged_open_count(), 1);
}

// --- paging ------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn readdir_pages_the_feed_with_synthetic_controls() {
    let t = test_ns();
    let ns = &t.ns;

    let hello = ns.lookup(NodeId::ROOT, "hello").await.unwrap();
    let feed = ns.lookup(hello.node, "feed").await.unwrap();

    // Page 0 carries the first provider items plus the @next/@all controls.
    let page0 = ns
        .readdir(feed.node, DirCursor::start(), 0)
        .await
        .expect("feed page 0");
    let names0: Vec<&str> = page0.entries.iter().map(|e| e.name.as_str()).collect();
    assert!(
        names0.contains(&"item-0") && names0.contains(&"item-1"),
        "{names0:?}"
    );
    assert!(
        names0.contains(&"@next") && names0.contains(&"@all"),
        "page 0 surfaces the synthetic pagination controls: {names0:?}"
    );
    let cursor0 = page0.next.expect("page 0 carries a continuation cursor");

    // The cursor advances: page 1 is the next raw provider page, no controls.
    let page1 = ns
        .readdir(feed.node, cursor0, 0)
        .await
        .expect("feed page 1");
    let names1: Vec<&str> = page1.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(
        names1,
        ["item-2", "item-3"],
        "continuation page is raw items"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn readdir_budget_buffers_overflow() {
    let t = test_ns();
    let ns = &t.ns;

    let hello = ns.lookup(NodeId::ROOT, "hello").await.unwrap();

    // A budget of 1 returns one entry and buffers the rest in the cursor, so the
    // full listing still drains across pages.
    let mut cursor = DirCursor::start();
    let mut seen = 0usize;
    let mut first_page_len = None;
    loop {
        let page = ns
            .readdir(hello.node, cursor, 1)
            .await
            .expect("budgeted page");
        if first_page_len.is_none() {
            first_page_len = Some(page.entries.len());
        }
        seen += page.entries.len();
        match page.next {
            Some(next) => cursor = next,
            None => break,
        }
        assert!(seen < 1000, "budgeted paging must terminate");
    }
    assert_eq!(
        first_page_len,
        Some(1),
        "budget caps the first page at one entry"
    );
    assert!(
        seen >= 17,
        "budgeted paging drains the whole /hello listing: {seen}"
    );
}

// An object STREAM face (`o.file("log").stream(...)`) now stamps its
// lookup/listing placeholder with `Deferred(Ranged)`
// (`omnifs-sdk` `object_dir_listing`, `ListingLeaf::is_stream`), so every
// `is_deferred_ranged` consumer (this namespace's read path, and the FUSE/NFS
// adapters' identical check on the same resolved meta) opens it through
// `open-file` instead of routing a whole-file `read-file` the provider
// rejects with InvalidInput.
#[tokio::test(flavor = "multi_thread")]
async fn stream_face_reads_through_open_file() {
    let t = test_ns();
    let ns = &t.ns;

    // /items/open/7/log is an object stream face: it opens through the provider
    // `open-file` import and must never be answered by `read-file` (the provider
    // rejects that with InvalidInput). The namespace's read path must detect the
    // deferred-ranged placeholder on the resolved node and take the open path.
    let items = ns.lookup(NodeId::ROOT, "items").await.expect("items");
    let open = ns.lookup(items.node, "open").await.expect("open");
    let seven = ns.lookup(open.node, "7").await.expect("7");
    let log = ns.lookup(seven.node, "log").await.expect("log");
    assert_eq!(log.kind, NsEntryKind::File);

    let answer = ns
        .read(log.node, 0, 16)
        .await
        .expect("stream face read goes through open-file");
    assert!(
        !answer.bytes.is_empty(),
        "stream face serves bytes through its ranged handle"
    );
}

// --- errors ------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn missing_child_is_not_found() {
    let t = test_ns();
    let ns = &t.ns;

    let hello = ns.lookup(NodeId::ROOT, "hello").await.unwrap();
    let err = ns
        .lookup(hello.node, "nonexistent")
        .await
        .expect_err("missing child errors");
    assert_eq!(err, NsError::NotFound);
}

// --- invalidation events -----------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn invalidation_bumps_epoch_and_notifies_subscriber() {
    let t = test_ns();
    let ns = &t.ns;

    let hello = ns.lookup(NodeId::ROOT, "hello").await.unwrap();
    let message = ns.lookup(hello.node, "message").await.unwrap();
    // Prime a read so the node carries a learned size and a real getattr answer.
    let _ = ns.read(message.node, 0, 64).await.expect("prime read");
    assert_eq!(
        ns.getattr(message.node).await.unwrap().size,
        13,
        "size learned before invalidation"
    );

    // Subscribe, then fire a provider invalidation for the message leaf.
    let mut events = ns.subscribe();
    t.runtime.apply_effects_for_test(
        &path_invalidation("/hello/message"),
        t.runtime.cache().current_generation(),
    );

    // Any op drains the pending invalidation before answering: getattr both
    // emits the event and re-resolves fresh state.
    let fresh = ns
        .getattr(message.node)
        .await
        .expect("getattr after invalidation");

    // The subscriber observes an InvalidateSubtree for the message node with a
    // bumped epoch (the background tick may have delivered it first; either way
    // the event is buffered for this receiver).
    let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
        .await
        .expect("an invalidation event arrives")
        .expect("event stream is live");
    match event {
        NsEvent::InvalidateSubtree { node, epoch } => {
            assert_eq!(node, message.node, "the message node is invalidated");
            assert!(epoch.0 >= 1, "the epoch is bumped, got {}", epoch.0);
        },
        NsEvent::AttrsChanged { .. } => panic!("expected InvalidateSubtree, got AttrsChanged"),
    }

    // Invalidation dropped the learned size, so the re-resolved answer falls
    // back to the provider's unknown-length placeholder: the answer is
    // recomputed, not served from the pre-invalidation state.
    assert_eq!(
        fresh.size, 1,
        "the post-invalidation getattr re-resolves fresh state"
    );
}
