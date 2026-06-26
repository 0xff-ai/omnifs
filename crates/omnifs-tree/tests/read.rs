//! Kernel-free read tests for omnifs-tree slice 2a: `Tree::read`, `Tree::open`
//! with `RangedHandle::read`, the read cache cascade, the canonical-not-copied
//! hybrid, and learned-size promotion, all against the in-tree
//! `test_provider.wasm` with NO fuser, NO mount, NO container, NO root.
//!
//! Reuses the omnifs-itest provider-loading harness (`RuntimeHarness` via
//! `make_runtime`), keeps an `Arc<Runtime>` clone so the test can inspect the
//! durable view cache and drive object invalidations directly, and wraps the
//! same `Runtime` in a `Tree` via `Tree::for_runtime`. This is the kernel-free
//! proof of the read-path DECISION logic FUSE otherwise carries in
//! `read.rs`/`read_helpers.rs`.
//!
//! The per-mount `op_gen` write fence's true-branch is NOT exercised here: it
//! only fires for a write whose `op_gen` predates a live tombstone for the
//! path's object, and `Tree::read` captures `op_gen` and writes the durable
//! cache in one synchronous poll for the canned (no-callout) test provider, so
//! the fenced ordering cannot be induced kernel-free. The fence call site is a
//! faithful port of the FUSE path, and the underlying `Store::write_fenced`
//! mechanism is covered by `omnifs-cache`'s `fence_rejects_stale_write`.
//!
//! Precondition: `just providers build` has produced
//! `target/wasm32-wasip2/release/test_provider.wasm` (`provider_wasm_path`
//! asserts this through the harness).

#![cfg(not(target_os = "wasi"))]
// Test docs reference protocol acronyms (FUSE, NFS) and type names as prose.
#![allow(clippy::doc_markdown)]

use std::sync::Arc;

use omnifs_cache::RecordKind;
use omnifs_core::path::Path;
use omnifs_core::view::{EntryMeta, FileAttrsCache, FilePayload, FileSize, ReadMode, Stability};
use omnifs_host::Runtime;
use omnifs_itest::{RuntimeHarness, make_engine, make_runtime};
use omnifs_tree::{Node, NodeBody, ReadResult, RequestCtx, Tree};
use omnifs_wit::provider::types::{Effects, Invalidation, PathOrPrefix};
use tempfile::TempDir;

/// Owns the harness temp dirs that must outlive the `Runtime`, the `Tree`
/// wrapping it, and a second `Arc<Runtime>` clone so the test can read the
/// durable view cache and apply object invalidations without going through the
/// `Tree` surface (which deliberately hides them).
struct TestTree {
    tree: Tree,
    runtime: Arc<Runtime>,
    _clone_dir: TempDir,
    _cache_dir: TempDir,
    _config_dir: TempDir,
}

fn test_tree() -> TestTree {
    let engine = make_engine();
    let RuntimeHarness {
        clone_dir,
        cache_dir,
        config_dir,
        runtime,
        ..
    } = make_runtime(&engine);
    let runtime = Arc::new(runtime);
    let tree = Tree::for_runtime(Arc::clone(&runtime), "test");
    TestTree {
        tree,
        runtime,
        _clone_dir: clone_dir,
        _cache_dir: cache_dir,
        _config_dir: config_dir,
    }
}

fn path(s: &str) -> Path {
    Path::parse(s).unwrap()
}

// --- Whole-file reads --------------------------------------------------------

/// `Tree::read` of a whole-file provider leaf returns the exact provider bytes,
/// promotes a learned exact size, and durably caches the payload.
#[tokio::test(flavor = "multi_thread")]
async fn read_whole_file_returns_provider_bytes() {
    let t = test_tree();
    let ctx = RequestCtx::default();

    let node = t
        .tree
        .resolve(&path("/hello/message"), &ctx)
        .await
        .expect("resolve /hello/message");

    let result = t.tree.read(&node, &ctx).await.expect("read /hello/message");
    let ReadResult::Bytes {
        data,
        attrs,
        content_type: _,
    } = result
    else {
        panic!("/hello/message must read as provider bytes, not a backing dir");
    };
    assert_eq!(data, b"Hello, world!");

    // The whole read learns the exact size from the returned bytes.
    let attrs = attrs.expect("a provider read carries post-read attrs");
    assert_eq!(attrs.size(), FileSize::Exact(13));

    // The payload is now in the durable view cache (immutable -> aux None).
    let record = t
        .runtime
        .cache()
        .cache_get(&path("/hello/message"), RecordKind::File, None)
        .expect("immutable whole-file read is durably cached");
    let payload = FilePayload::deserialize(&record.payload).expect("decode cached payload");
    assert_eq!(payload.content, b"Hello, world!");
}

/// A cold `Tree::read` populates the durable view cache, and a second read of
/// the same leaf returns the same bytes through the cache-consult cascade.
#[tokio::test(flavor = "multi_thread")]
async fn read_whole_file_second_read_hits_cache() {
    let t = test_tree();
    let ctx = RequestCtx::default();

    let node = t.tree.resolve(&path("/hello/lazy"), &ctx).await.unwrap();
    let first = t.tree.read(&node, &ctx).await.expect("cold read");
    let ReadResult::Bytes { data, .. } = first else {
        panic!("expected provider bytes");
    };
    assert_eq!(data, b"lazy\n");

    // The cold read durably cached "lazy\n" under the immutable (aux None) key.
    assert!(
        t.runtime
            .cache()
            .cache_get(&path("/hello/lazy"), RecordKind::File, None)
            .is_some(),
        "cold read must populate the durable view cache"
    );

    // Re-resolve so the node carries the size the first read learned (the inode
    // promotion the renderer otherwise owns); resolve alone keeps the listing
    // placeholder, which the cache-hit validator (Unknown size) still accepts.
    let warm = t.tree.resolve(&path("/hello/lazy"), &ctx).await.unwrap();
    let second = t.tree.read(&warm, &ctx).await.expect("warm read");
    let ReadResult::Bytes { data, .. } = second else {
        panic!("expected provider bytes");
    };
    assert_eq!(data, b"lazy\n");
}

/// An exact-zero-sized file is served empty with NO provider round trip: the
/// short-circuit is a pure decision on the node's projected `FileSize::Exact(0)`.
/// The renderer constructs the node (as it would from an empty inline
/// projection); the test plays that role with a node bound to a path that has
/// no provider route, so a regression that dropped the short-circuit and
/// dispatched to the provider would surface as a NotFound error here.
#[tokio::test(flavor = "multi_thread")]
async fn read_exact_zero_short_circuits() {
    let t = test_tree();
    let ctx = RequestCtx::default();

    let meta = EntryMeta::file(
        FileAttrsCache::inline(Vec::new(), Stability::Stable, None).expect("valid empty attrs"),
    );
    let node = Node::new(
        "test".to_string(),
        path("/hello/no-such-route"),
        meta,
        NodeBody::Provider,
    );

    let result = t.tree.read(&node, &ctx).await.expect("read empty file");
    let ReadResult::Bytes { data, attrs, .. } = result else {
        panic!("expected provider bytes");
    };
    assert!(data.is_empty(), "exact-0 file reads empty");
    assert_eq!(attrs.map(|a| a.size()), Some(FileSize::Exact(0)));
}

// --- Ranged reads ------------------------------------------------------------

/// Build the ranged `Node` a renderer hands to `Tree::open`. The provider's
/// lookup/list project a `listing_shape` placeholder (`Deferred(Full)`) for
/// every handler-routed file; the real `Deferred(Ranged)` byte source is only
/// known once the renderer has classified the leaf as ranged (mirroring how the
/// FUSE inode carries `Deferred(Ranged)` before `open_ranged_file` runs). The
/// test plays that renderer role: it constructs the node with the ranged byte
/// source so `Tree::open`'s precondition is met.
fn ranged_node(path_str: &str) -> Node {
    let meta = EntryMeta::file(
        FileAttrsCache::deferred(
            FileSize::Unknown,
            ReadMode::Ranged,
            Stability::Dynamic,
            None,
        )
        .expect("valid ranged attrs"),
    );
    Node::new("test".to_string(), path(path_str), meta, NodeBody::Provider)
}

/// `Tree::open` of a `Deferred(Ranged)` file yields a `RangedHandle` whose
/// `read` returns the provider chunk; an at-EOF read reports EOF.
#[tokio::test(flavor = "multi_thread")]
async fn open_ranged_then_read_chunks() {
    let t = test_tree();
    let ctx = RequestCtx::default();

    let node = ranged_node("/hello/ranged");
    let handle = t
        .tree
        .open(&node, &ctx)
        .await
        .expect("open ranged file")
        .expect("file is ranged");
    // open_file reports the provider's real attrs: Exact(26), Dynamic.
    assert_eq!(handle.attrs().size(), FileSize::Exact(26));
    assert_eq!(handle.attrs().stability(), Stability::Dynamic);

    // A mid-file chunk: "cdef" at offset 2, not EOF.
    let chunk = handle.read(2, 4).await.expect("read mid chunk");
    assert_eq!(chunk.bytes, b"cdef");
    assert!(!chunk.eof);
    assert!(chunk.learned_attrs.is_none());

    // An at-EOF read returns empty + eof; the exact-26 size is already known so
    // no new size is learned.
    let eof = handle.read(26, 8).await.expect("read at eof");
    assert!(eof.bytes.is_empty());
    assert!(eof.eof);

    handle.close().expect("close ranged handle");
}

/// An unknown-size ranged file learns its exact size from the EOF-short chunk,
/// surfacing `learned_attrs` so the renderer can promote st_size.
#[tokio::test(flavor = "multi_thread")]
async fn open_unknown_ranged_learns_size_on_eof() {
    let t = test_tree();
    let ctx = RequestCtx::default();

    // The unknown-ranged file is immutable + Unknown size.
    let meta = EntryMeta::file(
        FileAttrsCache::deferred(FileSize::Unknown, ReadMode::Ranged, Stability::Stable, None)
            .expect("valid unknown ranged attrs"),
    );
    let node = Node::new(
        "test".to_string(),
        path("/hello/unknown-ranged"),
        meta,
        NodeBody::Provider,
    );

    let handle = t
        .tree
        .open(&node, &ctx)
        .await
        .expect("open unknown-ranged")
        .expect("unknown-ranged is ranged");
    assert_eq!(handle.attrs().size(), FileSize::Unknown);

    // Reading from offset 8 returns the tail "size\n" and EOF. The exact size is
    // 8 + 5 = 13 ("unknown-size\n").
    let chunk = handle.read(8, 32).await.expect("read tail at eof");
    assert_eq!(chunk.bytes, b"size\n");
    assert!(chunk.eof);
    let learned = chunk
        .learned_attrs
        .expect("an EOF-short read on an Unknown-size file learns the size");
    assert_eq!(learned.size(), FileSize::Exact(13));

    handle.close().expect("close handle");
}

/// `Tree::open` probes `open_file` to discover the read mode the cheap lookup
/// placeholder omits. A non-ranged source reports `InvalidInput`/`NotFound`,
/// which surfaces as `Ok(None)` so the renderer falls through to a full read
/// rather than binding a ranged handle.
#[tokio::test(flavor = "multi_thread")]
async fn open_probe_returns_none_for_non_ranged_node() {
    let t = test_tree();
    let ctx = RequestCtx::default();

    let node = t.tree.resolve(&path("/hello/message"), &ctx).await.unwrap();
    let opened = t
        .tree
        .open(&node, &ctx)
        .await
        .expect("the open probe itself succeeds");
    assert!(
        opened.is_none(),
        "a non-ranged source must not open as a ranged handle"
    );
}

// --- Cache hybrid + durable-cache regressions --------------------------------

fn listing_invalidation(path_str: &str) -> Effects {
    Effects {
        canonical: Vec::new(),
        fs: Vec::new(),
        invalidations: vec![Invalidation::Listing(PathOrPrefix::Path(
            path_str.to_string(),
        ))],
    }
}

/// The Markdown representation of an item object: the one test-provider leaf
/// that is simultaneously (a) object-indexed (its path maps to the item's
/// logical id), (b) durably cacheable (Stable rendered representation ->
/// `durable_cache_aux` is `Some(None)`), and (c) Inline-rendered (NOT
/// `byte-source::canonical`, so it reaches the durable-cache write). It is the
/// positive counterpart to the identity `item.json` (canonical, never copied
/// into the view cache). The Dynamic-unversioned scalar fields
/// (`title`/`state`/`body`) are not durably cacheable at all.
const ITEM_MD: &str = "/items/open/7/item.md";

/// Prime the object-cache forward index for the item leaves by listing the
/// collection (the `item_list` handler emits a `store_canonical` whose
/// `view_leaves` include the item's leaves), then evict the freshly preloaded
/// view leaf so the next read is a cold render that reaches the durable-cache
/// write, while keeping the object index intact.
async fn prime_cold_item_md(t: &TestTree, ctx: &RequestCtx) {
    let items = t.tree.resolve(&path("/items/open"), ctx).await.unwrap();
    let _ = t
        .tree
        .list(&items, None, ctx)
        .await
        .expect("list /items/open");
    t.runtime.apply_effects_for_test(
        &listing_invalidation(ITEM_MD),
        t.runtime.cache().current_generation(),
    );
    assert!(
        t.runtime
            .cache()
            .cache_get(&path(ITEM_MD), RecordKind::File, None)
            .is_none(),
        "view leaf must be cold before the cold read"
    );
    assert!(
        t.runtime
            .cache()
            .cached_canonical_for(&path(ITEM_MD))
            .is_some(),
        "the item object must stay indexed for the read"
    );
}

/// A cold `Tree::read` of an object-indexed, Inline-rendered, Stable
/// representation (item.md) DOES land in the durable view cache. This is the
/// positive counterpart proving `finish_read`'s `from_canonical` guard is
/// selective: only identity bytes are withheld from the view cache, while a
/// rendered representation derived from the same object is durably cached.
#[tokio::test(flavor = "multi_thread")]
async fn read_item_md_is_durably_cached() {
    let t = test_tree();
    let ctx = RequestCtx::default();
    prime_cold_item_md(&t, &ctx).await;

    let node = t
        .tree
        .resolve(&path(ITEM_MD), &ctx)
        .await
        .expect("resolve item.md");
    let result = t.tree.read(&node, &ctx).await.expect("read item.md");
    let ReadResult::Bytes { data, attrs, .. } = result else {
        panic!("expected provider bytes");
    };
    assert_eq!(data, b"# Item 7\n\nBody 7\n");
    assert_eq!(
        attrs.map(|a| a.stability()),
        Some(Stability::Stable),
        "the Markdown representation is an immutable rendering"
    );

    assert!(
        t.runtime
            .cache()
            .cache_get(&path(ITEM_MD), RecordKind::File, None)
            .is_some(),
        "an Inline rendered representation must be durably cached"
    );
}

/// REGRESSION (canonical-not-copied hybrid): an identity `byte-source::canonical`
/// read is served from the object cache and is NEVER copied into the view cache,
/// even though it is Stable and would otherwise be durably cacheable. The
/// object cache is its sole home; copying it into the view cache would duplicate
/// the bytes across both stores. `item.json` is the item object's identity
/// representation, so a cold read answers `byte-source::canonical`. If
/// `finish_read` dropped its `from_canonical` guard, the canonical bytes would
/// land in the view cache and this test would catch it.
#[tokio::test(flavor = "multi_thread")]
async fn canonical_identity_read_is_not_copied_into_view_cache() {
    let json = "/items/open/7/item.json";
    let t = test_tree();
    let ctx = RequestCtx::default();

    // Prime the object index, then evict the preloaded view leaf so the read is
    // a cold render that answers from the canonical store.
    let items = t.tree.resolve(&path("/items/open"), &ctx).await.unwrap();
    let _ = t
        .tree
        .list(&items, None, &ctx)
        .await
        .expect("list /items/open");
    t.runtime.apply_effects_for_test(
        &listing_invalidation(json),
        t.runtime.cache().current_generation(),
    );
    assert!(
        t.runtime
            .cache()
            .cache_get(&path(json), RecordKind::File, None)
            .is_none(),
        "view leaf must be cold before the canonical read"
    );
    assert!(
        t.runtime
            .cache()
            .cached_canonical_for(&path(json))
            .is_some(),
        "the identity representation lives in the object cache"
    );

    let node = t
        .tree
        .resolve(&path(json), &ctx)
        .await
        .expect("resolve item.json");
    let result = t.tree.read(&node, &ctx).await.expect("read item.json");
    let ReadResult::Bytes { data, .. } = result else {
        panic!("expected provider bytes");
    };
    assert_eq!(
        data,
        br#"{"number":7,"title":"Item 7","body":"Body 7","state":"open"}"#
    );

    // The canonical bytes are served, but the view cache stays empty for this
    // identity leaf: the object cache is its sole home.
    assert!(
        t.runtime
            .cache()
            .cache_get(&path(json), RecordKind::File, None)
            .is_none(),
        "an identity byte-source::canonical read must NOT be copied into the view cache"
    );
    // The object cache still holds the canonical (it was the source, not evicted).
    assert!(
        t.runtime
            .cache()
            .cached_canonical_for(&path(json))
            .is_some(),
        "the canonical store remains the home of the identity bytes"
    );
}
