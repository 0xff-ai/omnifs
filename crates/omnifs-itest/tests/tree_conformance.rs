//! Slice 3: a kernel-free Tree-level CONFORMANCE harness.
//!
//! This is the provider-author verification loop (D9): it drives omnifs-engine tree's
//! neutral `resolve` / `list` / `read` / `open` surface against a real wasm
//! provider with NO fuser, NO mount, NO container, NO root. A provider author
//! (or a frontend author proving the neutral surface) runs these to confirm a
//! provider's projection without standing up a kernel adapter.
//!
//! It reuses the omnifs-itest provider-loading harness (`make_runtime`), so the
//! provider is loaded and executed exactly as the host does in production; only
//! the renderer (FUSE/NFS) is absent. The reusable helpers below are the three
//! the brief calls for:
//!
//!   (a) load a wasm provider into a `Runtime`             -> `tree_harness`
//!   (b) build a `Tree` over it                            -> `tree_harness`
//!   (c) assert resolve/list/read outcomes                 -> `ConformanceTree`
//!
//! The conformance set covers: root + nested dir listing, a whole-file read with
//! exact bytes, a ranged file, a cursored/paginated listing, a not-found, and
//! the write-fence/invalidation coherence `Tree::read` carries.
//!
//! Precondition: `just providers build` has produced
//! `target/wasm32-wasip2/release/test_provider.wasm` (`provider_wasm_path`
//! asserts this through the harness).

#![cfg(not(target_os = "wasi"))]
// Test docs reference protocol acronyms (FUSE, NFS) and type names as prose.
#![allow(clippy::doc_markdown)]

use std::sync::Arc;

use omnifs_core::path::Path;
use omnifs_engine::Engine;
use omnifs_engine::test_support::cache::RecordKind;
use omnifs_engine::view::{CachedCursor, FileSize, Stability};
use omnifs_engine::{
    Cursor, ListOutcome, Listing, Node, ReadResult, RequestCtx, ServingContext, Tree, TreeErrorKind,
};
use omnifs_itest::{RuntimeHarness, make_engine, make_runtime};
use omnifs_wit::provider::types::{Effects, Invalidation, PathOrPrefix};
use tempfile::TempDir;

// ===========================================================================
// Reusable conformance harness
// ===========================================================================

/// (a)+(b): a wasm provider loaded into a `Runtime`, wrapped in a `Tree` under a
/// fixed mount name. Owns the harness temp dirs that must outlive the `Runtime`
/// plus a second `Arc<Engine>` clone so a conformance test can drive object
/// invalidations directly (the write-fence/coherence case the `Tree` surface
/// deliberately hides).
pub struct ConformanceTree {
    tree: Tree,
    runtime: Arc<Engine>,
    ctx: RequestCtx,
    _clone_dir: TempDir,
    _cache_dir: TempDir,
    _config_dir: TempDir,
}

/// (a)+(b): load `test_provider.wasm` into a `Runtime` and build a `Tree` over
/// it under mount "test". The single entry a provider-author test calls before
/// driving `resolve` / `list` / `read`.
pub fn tree_harness() -> ConformanceTree {
    let engine = make_engine();
    let RuntimeHarness {
        clone_dir,
        cache_dir,
        config_dir,
        runtime,
        ..
    } = make_runtime(&engine);
    let runtime = Arc::new(runtime);
    let tree = Tree::new(ServingContext::single(
        "test".to_string(),
        Arc::clone(&runtime),
    ));
    ConformanceTree {
        tree,
        runtime,
        ctx: RequestCtx::default(),
        _clone_dir: clone_dir,
        _cache_dir: cache_dir,
        _config_dir: config_dir,
    }
}

fn path(s: &str) -> Path {
    Path::parse(s).unwrap()
}

impl ConformanceTree {
    // --- (c): resolve assertions --------------------------------------------

    /// Resolve `path` and assert it is a directory node. Returns the node so the
    /// caller can list it.
    pub async fn assert_dir(&self, path_str: &str) -> Node {
        let node = self
            .tree
            .resolve(&path(path_str), &self.ctx)
            .await
            .unwrap_or_else(|e| panic!("resolve {path_str} expected a directory: {e}"));
        assert!(node.is_dir(), "{path_str} must resolve to a directory");
        assert_eq!(node.mount(), "test");
        node
    }

    /// Resolve `path` and assert it is a file node. Returns the node so the
    /// caller can read it.
    pub async fn assert_file(&self, path_str: &str) -> Node {
        let node = self
            .tree
            .resolve(&path(path_str), &self.ctx)
            .await
            .unwrap_or_else(|e| panic!("resolve {path_str} expected a file: {e}"));
        assert!(node.is_file(), "{path_str} must resolve to a file");
        node
    }

    /// Resolve `path` and assert it is reported as not-found (a clean ENOENT,
    /// never an internal error). The negative oracle a renderer maps to ENOENT.
    pub async fn assert_not_found(&self, path_str: &str) {
        let err = self
            .tree
            .resolve(&path(path_str), &self.ctx)
            .await
            .expect_err("missing path must error");
        assert_eq!(
            err.kind,
            TreeErrorKind::NotFound,
            "{path_str} must surface as NotFound, got {err}"
        );
    }

    // --- (c): list assertions -----------------------------------------------

    /// List `node`, asserting it is a provider listing (not a subtree handoff).
    pub async fn list(&self, node: &Node, cursor: Option<Cursor>) -> Listing {
        match self
            .tree
            .list(node, cursor, &self.ctx)
            .await
            .unwrap_or_else(|e| panic!("list {} failed: {e}", node.path().as_str()))
        {
            ListOutcome::Listing(listing) => listing,
            ListOutcome::Subtree(dir) => {
                panic!(
                    "expected a provider listing, got a subtree handoff at {}",
                    dir.display()
                )
            },
        }
    }

    /// Resolve `path` (which must be a directory) and return its child names.
    pub async fn list_names(&self, path_str: &str) -> Vec<String> {
        let node = self.assert_dir(path_str).await;
        let listing = self.list(&node, None).await;
        listing.entries.into_iter().map(|e| e.name).collect()
    }

    // --- (c): read assertions -----------------------------------------------

    /// Resolve `path` (a whole-file leaf), read it, and assert the exact bytes.
    /// Returns the read result so the caller can inspect learned attrs.
    pub async fn assert_read(&self, path_str: &str, expected: &[u8]) -> ReadResult {
        let node = self.assert_file(path_str).await;
        let result = self
            .tree
            .read(&node, &self.ctx)
            .await
            .unwrap_or_else(|e| panic!("read {path_str} failed: {e}"));
        match &result {
            ReadResult::Bytes { data, .. } => {
                assert_eq!(data, expected, "{path_str} bytes mismatch");
            },
            ReadResult::Subtree(dir) => {
                panic!(
                    "expected provider bytes for {path_str}, got a subtree {}",
                    dir.display()
                )
            },
        }
        result
    }
}

// ===========================================================================
// Conformance set
// ===========================================================================

/// Root + nested directory listing: the mount root enumerates the provider's
/// top-level route families, and a nested directory enumerates its children.
/// Lookup is the authoritative name oracle, so the assertions check membership,
/// not order.
#[tokio::test(flavor = "multi_thread")]
async fn lists_root_and_nested_directories() {
    let t = tree_harness();

    let root = t.list_names("/").await;
    for expected in ["items", "hello", "scoped", "checkout", "dynamic"] {
        assert!(
            root.contains(&expected.to_string()),
            "root missing {expected}: {root:?}"
        );
    }

    let hello = t.list_names("/hello").await;
    for expected in ["message", "greeting", "ranged", "bundle", "feed"] {
        assert!(
            hello.contains(&expected.to_string()),
            "/hello missing {expected}: {hello:?}"
        );
    }

    // A nested directory one level deeper still resolves + lists.
    let items_open = t.list_names("/items/open").await;
    for expected in ["7", "8"] {
        assert!(
            items_open.contains(&expected.to_string()),
            "/items/open missing {expected}: {items_open:?}"
        );
    }
}

/// Regression for the chained file-face builder path:
/// `o.file("body").lazy().derive(f)` marks `body` lazy, without leaking the
/// lazy flag to neighboring leaves. Listing the object anchor should preload
/// eager derived leaves only.
#[tokio::test(flavor = "multi_thread")]
async fn lazy_derived_face_applies_to_the_declared_leaf_only() {
    let t = tree_harness();

    let item = t.assert_dir("/items/open/7").await;
    let listing = t.list(&item, None).await;
    let names = provider_entry_names(&listing);
    for expected in ["body", "state", "title"] {
        assert!(
            names.contains(&expected),
            "object listing missing {expected}: {names:?}"
        );
    }

    for eager in ["/items/open/7/state", "/items/open/7/title"] {
        assert!(
            t.runtime
                .cache()
                .cache_get(&path(eager), RecordKind::File, None)
                .is_some(),
            "eager derived leaf {eager} must be projected during anchor listing"
        );
    }

    assert!(
        t.runtime
            .cache()
            .cache_get(&path("/items/open/7/body"), RecordKind::File, None)
            .is_none(),
        "lazy body leaf must remain visible but not be preloaded"
    );
}

/// A whole-file read returns the provider's exact bytes and learns the exact
/// size from the returned buffer (the size promotion a renderer applies to
/// st_size / the NFSv4 change attribute).
#[tokio::test(flavor = "multi_thread")]
async fn reads_whole_file_exact_bytes() {
    let t = tree_harness();

    let result = t.assert_read("/hello/message", b"Hello, world!").await;
    let ReadResult::Bytes { attrs, .. } = result else {
        unreachable!("assert_read already proved Bytes");
    };
    let attrs = attrs.expect("a provider read carries post-read attrs");
    assert_eq!(
        attrs.size(),
        FileSize::Exact(13),
        "whole read learns the exact size"
    );

    // The read durably cached the immutable payload (aux None).
    assert!(
        t.runtime
            .cache()
            .cache_get(&path("/hello/message"), RecordKind::File, None)
            .is_some(),
        "an immutable whole-file read is durably cached"
    );
}

/// A ranged file opens to a `RangedHandle`, serves a mid-file chunk, and reports
/// EOF at end of file. The provider's reported size + stability survive `open`.
#[tokio::test(flavor = "multi_thread")]
async fn reads_ranged_file_in_chunks() {
    let t = tree_harness();

    // `/hello/ranged` is a `Deferred(Ranged)` leaf; a renderer constructs the
    // ranged node from the leaf's classification (the listing placeholder is
    // `Deferred(Full)` until the renderer classifies it as ranged, mirroring the
    // FUSE inode carrying `Deferred(Ranged)` before `open_ranged_file`). The
    // harness plays that renderer role.
    let node = ranged_node("/hello/ranged");
    let handle = t
        .tree
        .open(&node, &t.ctx)
        .await
        .expect("open ranged file")
        .expect("file is ranged");
    assert_eq!(handle.attrs().size(), FileSize::Exact(26));
    assert_eq!(handle.attrs().stability(), Stability::Dynamic);

    // A mid-file chunk: "cdef" at offset 2, not EOF.
    let chunk = handle.read(2, 4).await.expect("read mid chunk");
    assert_eq!(chunk.bytes, b"cdef");
    assert!(!chunk.eof);

    // An at-EOF read returns empty + eof.
    let eof = handle.read(26, 8).await.expect("read at eof");
    assert!(eof.bytes.is_empty());
    assert!(eof.eof);

    handle.close().expect("close ranged handle");
}

/// A cursored/paginated listing: `/hello/feed` yields two `item-*` entries per
/// page and carries a resume `Cursor` while pages remain. The conformance loop
/// drives the cursor forward through `Tree::list` and asserts the page contents,
/// proving `next_cursor` survives the neutral boundary (NFS turns a non-
/// exhaustive dynamic dir into a finite snapshot by draining exactly this loop).
#[tokio::test(flavor = "multi_thread")]
async fn lists_cursored_pages() {
    let t = tree_harness();
    let feed = t.assert_dir("/hello/feed").await;

    // Page 0: item-0, item-1, with a resume cursor to page 1.
    let page0 = t.list(&feed, None).await;
    assert_eq!(provider_entry_names(&page0), ["item-0", "item-1"]);
    let Some(Cursor(CachedCursor::Page(1))) = page0.next_cursor else {
        panic!(
            "page 0 must carry a resume cursor to page 1, got {:?}",
            page0.next_cursor
        );
    };

    // Page 1: item-2, item-3, with a resume cursor to page 2.
    let page1 = t.list(&feed, Some(Cursor(CachedCursor::Page(1)))).await;
    assert_eq!(provider_entry_names(&page1), ["item-2", "item-3"]);
    let Some(Cursor(CachedCursor::Page(2))) = page1.next_cursor else {
        panic!(
            "page 1 must carry a resume cursor to page 2, got {:?}",
            page1.next_cursor
        );
    };

    // Page 2 is terminal: item-4, item-5, no resume cursor.
    let page2 = t.list(&feed, Some(Cursor(CachedCursor::Page(2)))).await;
    assert_eq!(provider_entry_names(&page2), ["item-4", "item-5"]);
    assert!(
        page2.next_cursor.is_none(),
        "the terminal page clears the cursor, got {:?}",
        page2.next_cursor
    );
}

/// An unrouted name surfaces as a clean `NotFound`, never an internal error: the
/// negative oracle a renderer maps to ENOENT. Probes a missing leaf, a missing
/// top-level family, and a path-capture that fails its segment validator (the
/// `{filter}` axis only accepts "open"/"all"), proving a parse rejection falls
/// through to ENOENT rather than an internal error.
///
/// Note: the item object's own `load`-time NotFound (e.g. item 404) does NOT
/// surface here. Its route is registered with static leaves, so `/items/open/404`
/// and `/items/open/404/item.json` resolve to directory/file nodes; the
/// `Load::NotFound` only fires on `read`, where the provider returns
/// `ErrorKind::NotFound`. Slice 1's `From<host::Error>` still maps that to
/// `TreeErrorKind::Internal` (error.rs: "a richer mapping lands [later]"), so a
/// read-time NotFound is intentionally out of this conformance set's scope.
#[tokio::test(flavor = "multi_thread")]
async fn resolves_unrouted_path_as_not_found() {
    let t = tree_harness();
    t.assert_not_found("/hello/nonexistent").await;
    t.assert_not_found("/no-such-top-level").await;
    t.assert_not_found("/items/bogus-filter").await;
}

/// Write-fence / invalidation coherence carried by `Tree::read`: a cold read
/// durably caches the rendered payload, and a listing invalidation for that path
/// evicts the durable view leaf so the NEXT read re-renders from the provider
/// rather than serving the stale cached bytes.
///
/// This is the Tree-observable face of the per-mount `op_gen` fence. The fence's
/// raw stale-write rejection (a write whose `op_gen` predates a live tombstone)
/// cannot be induced kernel-free: `Tree::read` captures `op_gen` and writes the
/// durable cache in one synchronous poll for the canned (no-callout) provider,
/// so the fenced ordering never arises. The underlying `Store::write_fenced`
/// mechanism is covered by engine cache's `fence_rejects_stale_write`; this
/// asserts the coherence outcome the fence exists to guarantee.
#[tokio::test(flavor = "multi_thread")]
async fn invalidation_evicts_cached_read() {
    let t = tree_harness();

    // Cold read: durably caches "Hi there!\n" under the immutable (aux None) key.
    t.assert_read("/hello/greeting", b"Hi there!\n").await;
    assert!(
        t.runtime
            .cache()
            .cache_get(&path("/hello/greeting"), RecordKind::File, None)
            .is_some(),
        "the cold read must populate the durable view cache"
    );

    // A listing invalidation for the path bumps the per-mount generation and
    // evicts the view leaf (the coherence the read-path fence protects).
    t.runtime.apply_effects_for_test(
        &listing_invalidation("/hello/greeting"),
        t.runtime.cache().current_generation(),
    );
    assert!(
        t.runtime
            .cache()
            .cache_get(&path("/hello/greeting"), RecordKind::File, None)
            .is_none(),
        "the invalidation must evict the durable view leaf"
    );

    // The next read re-renders from the provider and re-populates the cache; the
    // bytes are still correct (immutable greeting), proving the read path goes
    // back to the provider on a miss rather than serving evicted bytes.
    t.assert_read("/hello/greeting", b"Hi there!\n").await;
    assert!(
        t.runtime
            .cache()
            .cache_get(&path("/hello/greeting"), RecordKind::File, None)
            .is_some(),
        "the re-render must re-populate the durable view cache"
    );
}

// ===========================================================================
// Local helpers
// ===========================================================================

/// Build the ranged `Node` a renderer hands to `Tree::open`. The provider's
/// lookup/list project a `listing_shape` placeholder (`Deferred(Full)`) for
/// every handler-routed file; the real `Deferred(Ranged)` byte source is only
/// known once the renderer has classified the leaf as ranged. The harness plays
/// that renderer role so `Tree::open`'s precondition is met.
fn ranged_node(path_str: &str) -> Node {
    use omnifs_engine::view::{FileAttrsCache, ReadMode};
    let attrs = FileAttrsCache::deferred(
        FileSize::Unknown,
        ReadMode::Ranged,
        Stability::Dynamic,
        None,
    )
    .expect("ranged attrs");
    Node::provider_file("test".to_string(), path(path_str), Some(attrs))
}

fn provider_entry_names(listing: &Listing) -> Vec<&str> {
    listing
        .entries
        .iter()
        .filter(|entry| !entry.is_synthetic())
        .map(|entry| entry.name.as_str())
        .collect()
}

fn listing_invalidation(path_str: &str) -> Effects {
    Effects {
        canonical: Vec::new(),
        fs: Vec::new(),
        invalidations: vec![Invalidation::Listing(PathOrPrefix::Path(
            path_str.to_string(),
        ))],
    }
}
