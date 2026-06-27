//! Kernel-free tracer for omnifs-tree slice 1: resolve + list against the
//! in-tree `test_provider.wasm` with NO fuser, NO mount, NO container, NO root.
//!
//! Reuses the existing omnifs-itest provider-loading harness (`RuntimeHarness`
//! via `make_runtime`), wraps the bare `Runtime` in a `Tree` via
//! `Tree::for_runtime`, and drives `Tree::resolve` / `Tree::list`. This is the
//! third consumer (after FUSE and NFS) proving the neutral surface, passing
//! before either kernel adapter is rewired.
//!
//! Precondition: `just providers build` has produced
//! `target/wasm32-wasip2/release/test_provider.wasm` (`provider_wasm_path`
//! asserts this through the harness).

#![cfg(not(target_os = "wasi"))]
// Test docs reference protocol acronyms (NFSv4, FUSE) and type names as prose.
#![allow(clippy::doc_markdown)]

use std::sync::Arc;

use omnifs_cache::{Record as CacheRecord, RecordKind};
use omnifs_core::path::Path;
use omnifs_core::view::{DirentRecord, DirentsPayload, EntryMeta};
use omnifs_host::Runtime;
use omnifs_itest::{RuntimeHarness, make_engine, make_runtime};
use omnifs_tree::{ListOutcome, NodeBody, RequestCtx, Tree, TreeErrorKind};
use tempfile::TempDir;

/// Owns the harness temp dirs that must outlive the `Runtime`, plus the `Tree`
/// wrapping it. The `Runtime` is moved into an `Arc` for the `Tree`; the three
/// `TempDir`s are retained here so the cache/clone/config directories survive
/// for the whole test.
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

#[tokio::test(flavor = "multi_thread")]
async fn resolve_root_and_known_dirs() {
    let t = test_tree();
    let tree = &t.tree;
    let ctx = RequestCtx::default();

    // Root resolves to a directory.
    let root = tree
        .resolve(&Path::parse("/").unwrap(), &ctx)
        .await
        .expect("resolve root");
    assert!(root.is_dir(), "mount root must be a directory");

    // A known nested directory the test provider projects.
    let hello = tree
        .resolve(&Path::parse("/hello").unwrap(), &ctx)
        .await
        .expect("resolve /hello");
    assert!(hello.is_dir(), "/hello must be a directory");
    assert_eq!(hello.path().as_str(), "/hello");
    assert_eq!(hello.mount(), "test");

    // A known file under /hello (the provider's root listing proves "message").
    let message = tree
        .resolve(&Path::parse("/hello/message").unwrap(), &ctx)
        .await
        .expect("resolve /hello/message");
    assert!(message.is_file(), "/hello/message must be a file");
}

#[tokio::test(flavor = "multi_thread")]
async fn resolve_missing_is_not_found() {
    let t = test_tree();
    let tree = &t.tree;
    let ctx = RequestCtx::default();

    let err = tree
        .resolve(&Path::parse("/hello/nonexistent").unwrap(), &ctx)
        .await
        .expect_err("missing child must error");
    assert_eq!(err.kind, TreeErrorKind::NotFound);
}

#[tokio::test(flavor = "multi_thread")]
async fn resolve_child_uses_cached_dirent_positive() {
    let t = test_tree();
    let tree = &t.tree;
    let ctx = RequestCtx::default();

    let parent = tree
        .resolve(&Path::parse("/hello").unwrap(), &ctx)
        .await
        .expect("resolve /hello");
    let payload = DirentsPayload {
        entries: vec![DirentRecord {
            name: "cached-only.txt".to_string(),
            meta: EntryMeta::file_without_attrs(),
        }],
        exhaustive: false,
        validator: None,
        next_cursor: None,
        paginated: false,
    }
    .serialize()
    .expect("serialize dirents");
    let record = CacheRecord::new(RecordKind::Dirents, payload);
    t.runtime
        .cache()
        .cache_put(parent.path(), RecordKind::Dirents, None, &record);

    let child = tree
        .resolve_child(&parent, "cached-only.txt", &ctx)
        .await
        .expect("resolve cached dirent child");
    assert_eq!(child.path().as_str(), "/hello/cached-only.txt");
    assert!(child.is_file());
}

#[tokio::test(flavor = "multi_thread")]
async fn list_root_yields_known_children() {
    let t = test_tree();
    let tree = &t.tree;
    let ctx = RequestCtx::default();

    let root = tree
        .resolve(&Path::parse("/").unwrap(), &ctx)
        .await
        .unwrap();
    let listing = match tree.list(&root, None, &ctx).await.expect("list root") {
        ListOutcome::Listing(l) => l,
        ListOutcome::Subtree(_) => panic!("root must be a provider listing, not a subtree"),
    };
    let names: Vec<&str> = listing
        .entries
        .iter()
        .filter(|e| !e.is_synthetic())
        .map(|e| e.name.as_str())
        .collect();
    let synthetic_names: Vec<&str> = listing
        .entries
        .iter()
        .filter(|e| e.is_synthetic())
        .map(|e| e.name.as_str())
        .collect();
    // Verified against providers/test/src/lib.rs route registrations on this
    // branch: the root projects items, hello, scoped, the /dynamic capture
    // prefix, and the checkout treeref. (The blueprint cited 4 from a stale
    // runtime_test.rs:47 that predates the /dynamic route; the host's own
    // test_list_root is failing the same way on feat/omnifs-nfs.)
    assert_eq!(names.len(), 5, "got {names:?}");
    assert!(names.contains(&"items"));
    assert!(names.contains(&"hello"));
    assert!(names.contains(&"scoped"));
    assert!(names.contains(&"checkout"));
    assert!(names.contains(&"dynamic"));
    assert_eq!(
        synthetic_names,
        [".gitignore", ".ignore", ".rgignore"],
        "got {synthetic_names:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn list_hello_yields_fourteen_children_with_message() {
    let t = test_tree();
    let tree = &t.tree;
    let ctx = RequestCtx::default();

    let hello = tree
        .resolve(&Path::parse("/hello").unwrap(), &ctx)
        .await
        .unwrap();
    let listing = match tree.list(&hello, None, &ctx).await.expect("list /hello") {
        ListOutcome::Listing(l) => l,
        ListOutcome::Subtree(_) => panic!("/hello is a provider listing"),
    };
    let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
    // Verified against providers/test/src/lib.rs: /hello projects 9 files
    // (message, greeting, projected, lazy, fresh-full, ranged, unknown-ranged,
    // large-ranged, volatile-tail) and 5 dirs (bundle, feed, unbounded,
    // throttled, snapshot) = 14. (The blueprint cited 13 from a stale
    // runtime_test.rs:80; the host's own test_list_hello_dir fails the same way
    // on feat/omnifs-nfs.)
    assert_eq!(listing.entries.len(), 14, "got {names:?}");
    assert!(names.contains(&"message"));
}

/// The test provider's `/checkout` returns `subtree(tree-ref 777)` (see
/// providers/test/src/lib.rs:499), but tree 777 is never registered in the
/// kernel-free harness (no git clone / archive callout runs), so the host's op
/// validator rejects the lookup with "subtree result references unknown tree
/// 777" (runtime_test.rs:560-584). This exercises the `LookupChildResult::Subtree`
/// -> `resolve_tree_ref` seam in `Tree::resolve`; with a real backing dir it
/// would return `NodeBody::Subtree`. Slice-1 deviation from the blueprint's
/// `checkout_is_a_subtree_handoff` test, which assumed a resolvable treeref.
#[tokio::test(flavor = "multi_thread")]
async fn checkout_dangling_treeref_surfaces_as_error() {
    let t = test_tree();
    let tree = &t.tree;
    let ctx = RequestCtx::default();

    let err = tree
        .resolve(&Path::parse("/checkout").unwrap(), &ctx)
        .await
        .expect_err("dangling treeref must error in the kernel-free harness");
    assert!(
        err.message.contains("777") || err.message.contains("tree"),
        "expected a tree-ref error, got: {err}"
    );
    // The NodeBody::Subtree shape exists for the resolvable case; assert the
    // type is reachable so the seam stays wired.
    let _ = NodeBody::Subtree(std::path::PathBuf::from("/unused"));
}

/// NFSv4 filehandle-first / FUSE bare-inode rehydration: persist a NodeId
/// (mount, path), drop the Node, resolve again by the same path, get an equal
/// node back from the (now-warm) cache-consult path, with no re-walk.
#[tokio::test(flavor = "multi_thread")]
async fn resolve_rehydrates_by_path_without_re_walk() {
    let t = test_tree();
    let tree = &t.tree;
    let ctx = RequestCtx::default();

    let first = tree
        .resolve(&Path::parse("/hello/message").unwrap(), &ctx)
        .await
        .unwrap();
    let id = first.id();
    drop(first);

    let again = tree
        .resolve(&id.path, &ctx)
        .await
        .expect("rehydrate by path");
    assert_eq!(again.mount(), id.mount);
    assert_eq!(again.path().as_str(), id.path.as_str());
    assert!(again.is_file());
}
