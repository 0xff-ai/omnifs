//! Kernel-free tests for the listing and lookup policy `Tree` owns end to end,
//! with no fuser, mount, container, or root privileges.
//!
//! Covers the renderer-neutral synthetic entries (`@next`/`@all` pagination
//! controls and the mount-root `.gitignore`/`.ignore`/`.rgignore` files), the
//! cursor-driven pagination drain, the serve-cached listing path, and the
//! negative-cache short-circuit. Keeping these policies in `Tree` gives every
//! frontend the same behavior.
//!
//! Reuses the omnifs-itest provider-loading harness (`make_runtime`), keeps an
//! `Arc<Engine>` clone so a test can inspect the negative index and the cached
//! dirents directly, and wraps the same `Engine` in a `Tree` via
//! `ServingContext::single`.
//!
//! Precondition: `just build providers` has produced
//! `target/wasm32-wasip2/release/test_provider.wasm` (`provider_wasm_path`
//! asserts this through the harness).

#![cfg(not(target_os = "wasi"))]
// Test docs reference protocol acronyms (FUSE, NFS) and type names as prose.
#![allow(clippy::doc_markdown)]

use std::sync::Arc;

use omnifs_core::path::Path;
use omnifs_engine::test_support::cache::{Record as CacheRecord, RecordKind};
use omnifs_engine::test_support::clock::now_millis;
use omnifs_engine::test_support::{PaginationControl, Synthetic, SyntheticContent};
use omnifs_engine::view::{CachedCursor, DirentRecord, DirentsPayload, EntryMeta};
use omnifs_engine::{
    Cursor, Engine, ListOutcome, Listing, Node, ReadResult, RequestCtx, ServingContext, Tree,
    TreeErrorKind,
};
use omnifs_itest::{RuntimeHarness, make_engine, make_initialized_runtime, make_runtime};
use tempfile::TempDir;

struct TestTree {
    tree: Tree,
    runtime: Arc<Engine>,
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
    let tree = Tree::new(ServingContext::single(
        "test".to_string(),
        Arc::clone(&runtime),
    ));
    TestTree {
        tree,
        runtime,
        _clone_dir: clone_dir,
        _cache_dir: cache_dir,
        _config_dir: config_dir,
    }
}

fn test_tree_with_config(config: &str) -> TestTree {
    let RuntimeHarness {
        clone_dir,
        cache_dir,
        config_dir,
        runtime,
        ..
    } = make_initialized_runtime(config);
    let engine = Arc::new(runtime);
    let tree = Tree::new(ServingContext::single(
        "test".to_string(),
        Arc::clone(&engine),
    ));
    TestTree {
        tree,
        runtime: engine,
        _clone_dir: clone_dir,
        _cache_dir: cache_dir,
        _config_dir: config_dir,
    }
}

fn path(s: &str) -> Path {
    Path::parse(s).unwrap()
}

async fn listing(t: &TestTree, node: &Node, cursor: Option<Cursor>, ctx: &RequestCtx) -> Listing {
    match t.tree.list(node, cursor, ctx).await.expect("list") {
        ListOutcome::Listing(l) => l,
        ListOutcome::Subtree(_) => panic!("expected a provider listing, got a subtree handoff"),
    }
}

fn provider_names(listing: &Listing) -> Vec<&str> {
    listing
        .entries
        .iter()
        .filter(|entry| !entry.is_synthetic())
        .map(|entry| entry.name.as_str())
        .collect()
}

fn synthetic_entries(listing: &Listing) -> Vec<&omnifs_engine::Entry> {
    listing
        .entries
        .iter()
        .filter(|entry| entry.is_synthetic())
        .collect()
}

fn synthetic_names(listing: &Listing) -> Vec<&str> {
    listing
        .entries
        .iter()
        .filter(|entry| entry.is_synthetic())
        .map(|entry| entry.name.as_str())
        .collect()
}

fn cached_dirents(runtime: &Engine, path_str: &str) -> Option<DirentsPayload> {
    let record = runtime
        .cache()
        .cache_get(&path(path_str), RecordKind::Dirents, None)?;
    DirentsPayload::deserialize(&record.payload)
}

// --- Pagination controls -----------------------------------------------------

/// A first-page browse listing of a paged directory carries the `@next`/`@all`
/// pagination controls as renderer-neutral synthetic entries, and surfaces the
/// resume cursor. The persisted dirents
/// record carries the control records so a later cached serve and a control
/// lookup still find them.
#[tokio::test(flavor = "multi_thread")]
async fn list_emits_pagination_controls() {
    let t = test_tree();
    let ctx = RequestCtx;
    let feed = t.tree.resolve(&path("/hello/feed"), &ctx).await.unwrap();

    let page0 = listing(&t, &feed, None, &ctx).await;
    // The provider entries stay raw: page 0 is item-0, item-1, no controls.
    assert_eq!(provider_names(&page0), ["item-0", "item-1"]);
    // The controls are synthetic entries, presented identically for every
    // renderer.
    let mut control_names = synthetic_names(&page0);
    control_names.sort_unstable();
    assert_eq!(control_names, ["@all", "@next"]);
    for entry in synthetic_entries(&page0) {
        let syn = entry.synthetic_kind().expect("a control is synthetic");
        let want = if entry.name == "@next" {
            PaginationControl::Next
        } else {
            PaginationControl::All
        };
        assert_eq!(
            syn,
            &Synthetic {
                content: SyntheticContent::PaginationControl(want),
            }
        );
    }
    // The resume cursor survives the boundary.
    assert_eq!(page0.next_cursor, Some(Cursor(CachedCursor::Page(1))));
    assert!(!page0.exhaustive);

    // The persisted dirents record carries the control records.
    let dirents = cached_dirents(&t.runtime, "/hello/feed").expect("paged dir caches dirents");
    assert!(dirents.entries.iter().any(|e| e.name == "@next"));
    assert!(dirents.entries.iter().any(|e| e.name == "@all"));
    assert!(dirents.paginated);
}

/// A non-paged directory carries NO pagination controls: the synthetic entries
/// are empty for an exhaustive listing.
#[tokio::test(flavor = "multi_thread")]
async fn list_non_paged_dir_has_no_controls() {
    let t = test_tree();
    let ctx = RequestCtx;
    let hello = t.tree.resolve(&path("/hello"), &ctx).await.unwrap();

    let page = listing(&t, &hello, None, &ctx).await;
    assert!(
        synthetic_entries(&page).is_empty(),
        "a non-paged dir emits no controls, got {:?}",
        synthetic_names(&page)
    );
}

/// An explicit-cursor continuation is a raw page drain: it advances the feed
/// page by page through `Tree::list`, carries NO synthetic entries, and clears
/// the cursor on the terminal page. This is the cursor-driven pagination NFS
/// uses to flatten a dynamic dir into a finite snapshot.
#[tokio::test(flavor = "multi_thread")]
async fn list_paginates_with_cursor() {
    let t = test_tree();
    let ctx = RequestCtx;
    let feed = t.tree.resolve(&path("/hello/feed"), &ctx).await.unwrap();

    let page0 = listing(&t, &feed, None, &ctx).await;
    assert_eq!(provider_names(&page0), ["item-0", "item-1"]);
    let cursor0 = page0.next_cursor.expect("page 0 carries a cursor");

    let page1 = listing(&t, &feed, Some(cursor0), &ctx).await;
    assert_eq!(provider_names(&page1), ["item-2", "item-3"]);
    assert!(
        synthetic_entries(&page1).is_empty(),
        "a continuation page carries no synthetic controls"
    );
    let cursor1 = page1.next_cursor.expect("page 1 carries a cursor");
    assert_eq!(cursor1, Cursor(CachedCursor::Page(2)));

    let page2 = listing(&t, &feed, Some(cursor1), &ctx).await;
    assert_eq!(provider_names(&page2), ["item-4", "item-5"]);
    assert!(synthetic_entries(&page2).is_empty());
    assert!(
        page2.next_cursor.is_none(),
        "the terminal page clears the cursor, got {:?}",
        page2.next_cursor
    );
}

/// A pagination control resolves to a synthetic node whenever the parent's
/// cached dirents carry it, and reading it runs the accumulating pagination:
/// `@next` advances exactly one page, growing the parent's cached dirents, and
/// the control read returns a one-line status with a learned exact size so
/// `cat` reads the whole message.
#[tokio::test(flavor = "multi_thread")]
async fn read_next_control_advances_one_page() {
    let t = test_tree();
    let ctx = RequestCtx;
    let feed = t.tree.resolve(&path("/hello/feed"), &ctx).await.unwrap();

    // Prime the paged listing so the parent's dirents carry the controls.
    let _ = listing(&t, &feed, None, &ctx).await;

    // The control resolves to a synthetic node.
    let next = t
        .tree
        .resolve(&path("/hello/feed/@next"), &ctx)
        .await
        .expect("@next resolves while the feed pages");
    assert!(next.is_synthetic());
    assert!(next.is_file());

    // Reading it advances exactly one page and reports the growth.
    let result = t.tree.read(&next, &ctx).await.expect("read @next");
    let ReadResult::Bytes { data, attrs, .. } = result else {
        panic!("a control read returns bytes");
    };
    let status = String::from_utf8(data).unwrap();
    assert!(
        status.contains("+2 entries") && status.contains("more available"),
        "got {status:?}"
    );
    // The learned exact size matches the status length so `cat` reads it whole.
    let attrs = attrs.expect("a control read carries learned attrs");
    assert_eq!(
        attrs.size(),
        omnifs_engine::view::FileSize::Exact(status.len() as u64)
    );

    // The parent's accumulated dirents grew by the new page.
    let dirents = cached_dirents(&t.runtime, "/hello/feed").expect("feed dirents present");
    for expected in ["item-0", "item-1", "item-2", "item-3"] {
        assert!(
            dirents.entries.iter().any(|e| e.name == expected),
            "accumulated dirents missing {expected}: {:?}",
            dirents.entries.iter().map(|e| &e.name).collect::<Vec<_>>()
        );
    }
}

/// `@all` advances the feed to exhaustion. The cursor clears, and a FRESH
/// listing stops naming either control, but the control names themselves keep
/// resolving and reading (as a no-op): presence in an already-served listing
/// must never regress to ENOENT. This is the converse of the documented
/// listing-authority rule (absence from a non-exhaustive listing is never
/// ENOENT either), covered at the itest/Tree level in
/// `omnifs-itest`'s `pagination_exhaustive::stale_snapshot_controls_resolve_after_exhaustion`.
#[tokio::test(flavor = "multi_thread")]
async fn read_all_control_exhausts_then_control_still_resolves() {
    let t = test_tree();
    let ctx = RequestCtx;
    let feed = t.tree.resolve(&path("/hello/feed"), &ctx).await.unwrap();
    let _ = listing(&t, &feed, None, &ctx).await;

    let all = t
        .tree
        .resolve(&path("/hello/feed/@all"), &ctx)
        .await
        .expect("@all resolves while the feed pages");
    let result = t.tree.read(&all, &ctx).await.expect("read @all");
    let ReadResult::Bytes { data, .. } = result else {
        panic!("a control read returns bytes");
    };
    assert!(
        String::from_utf8(data).unwrap().contains("complete"),
        "@all drains to completion"
    );

    // The feed is exhausted (no cursor), but the accumulated dirents keep the
    // control records so a name already resolved keeps resolving.
    let dirents = cached_dirents(&t.runtime, "/hello/feed").expect("feed dirents present");
    assert!(dirents.next_cursor.is_none());
    assert!(dirents.entries.iter().any(|e| e.name == "@next"));
    assert!(dirents.entries.iter().any(|e| e.name == "@all"));

    // A FRESH listing stops naming either control.
    let fresh = listing(&t, &feed, None, &ctx).await;
    assert!(
        synthetic_entries(&fresh).is_empty(),
        "a fresh listing hides the controls once exhausted, got {:?}",
        synthetic_names(&fresh)
    );

    // The control name still resolves and reads as a no-op, not NotFound.
    let next = t
        .tree
        .resolve(&path("/hello/feed/@next"), &ctx)
        .await
        .expect("an exhausted control still resolves");
    assert!(next.is_synthetic());
    let result = t.tree.read(&next, &ctx).await.expect("read @next");
    let ReadResult::Bytes { data, .. } = result else {
        panic!("a control read returns bytes");
    };
    assert_eq!(
        String::from_utf8(data).unwrap(),
        "no more pages\n",
        "reading an exhausted control is a no-op, not an error"
    );
}

// --- Root ignore files -------------------------------------------------------

/// The mount root carries the `.gitignore`/`.ignore`/`.rgignore` ignore files as
/// synthetic entries, each serving fixed ignore content so ignore-respecting
/// tree walks skip the `@`-prefixed controls and generated README leaves. They
/// are synthetic and resolve + read through `Tree`.
#[tokio::test(flavor = "multi_thread")]
async fn root_ignore_synthesized() {
    let t = test_tree();
    let ctx = RequestCtx;
    let root = t.tree.resolve(&path("/"), &ctx).await.unwrap();

    let listing = listing(&t, &root, None, &ctx).await;
    let mut ignore_names = synthetic_names(&listing);
    ignore_names.sort_unstable();
    assert_eq!(ignore_names, [".gitignore", ".ignore", ".rgignore"]);
    // The provider entries do not contain the ignore files.
    for n in [".gitignore", ".ignore", ".rgignore"] {
        assert!(
            !listing
                .entries
                .iter()
                .any(|e| e.name == n && !e.is_synthetic()),
            "ignore files are synthetic, not provider entries"
        );
    }

    // Each ignore file resolves to a synthetic node and reads the fixed content.
    let gitignore = t
        .tree
        .resolve(&path("/.gitignore"), &ctx)
        .await
        .expect(".gitignore is synthesized at the mount root");
    assert!(gitignore.is_synthetic());
    let result = t
        .tree
        .read(&gitignore, &ctx)
        .await
        .expect("read .gitignore");
    let ReadResult::Bytes { data, .. } = result else {
        panic!("ignore file reads as bytes");
    };
    assert_eq!(data, b"@*\n/README.md\n/*/README.md\n");
}

/// A non-root directory does NOT synthesize ignore files (they belong only at
/// the mount root), and resolving one there is NotFound.
#[tokio::test(flavor = "multi_thread")]
async fn root_ignore_not_synthesized_below_root() {
    let t = test_tree();
    let ctx = RequestCtx;
    let hello = t.tree.resolve(&path("/hello"), &ctx).await.unwrap();

    let listing = listing(&t, &hello, None, &ctx).await;
    for n in [".gitignore", ".ignore", ".rgignore"] {
        assert!(
            !synthetic_entries(&listing).iter().any(|e| e.name == n),
            "ignore files belong only at the mount root"
        );
    }

    let err = t
        .tree
        .resolve(&path("/hello/.gitignore"), &ctx)
        .await
        .expect_err("a non-root ignore file is not synthesized");
    assert_eq!(err.kind, TreeErrorKind::NotFound);
}

/// A provider collision at the mount root is filtered from a fresh listing and
/// replaced by exactly one host-owned synthetic file.
#[tokio::test(flavor = "multi_thread")]
async fn root_ignore_provider_collision_is_host_owned() {
    let t = test_tree_with_config(
        r#"{"provider":"test_provider.wasm","mount":"test","config":{"root_ignore":true}}"#,
    );
    let ctx = RequestCtx::default();
    let root = t.tree.resolve(&path("/"), &ctx).await.unwrap();
    let listing = listing(&t, &root, None, &ctx).await;

    for name in [".gitignore", ".ignore", ".rgignore"] {
        let matching: Vec<_> = listing.entries.iter().filter(|e| e.name == name).collect();
        assert_eq!(matching.len(), 1, "root listing must contain one {name}");
        assert!(matching[0].is_synthetic(), "{name} must be synthetic");
    }
    let node = t.tree.resolve(&path("/.gitignore"), &ctx).await.unwrap();
    assert!(node.is_file() && node.is_synthetic());
}

/// Cached provider-shaped metadata cannot change a root ignore file's kind,
/// and repeated path resolution rehydrates the same synthetic file identity.
#[tokio::test(flavor = "multi_thread")]
async fn root_ignore_cached_directory_is_replaced_before_lookup() {
    let t = test_tree();
    let ctx = RequestCtx::default();
    let root = t.tree.resolve(&path("/"), &ctx).await.unwrap();
    let payload = DirentsPayload {
        entries: vec![
            DirentRecord {
                name: ".gitignore".to_string(),
                meta: EntryMeta::directory(),
            },
            DirentRecord {
                name: ".ignore".to_string(),
                meta: EntryMeta::directory(),
            },
        ],
        exhaustive: true,
        validator: None,
        next_cursor: None,
        paginated: false,
    }
    .serialize()
    .expect("serialize root dirents");
    t.runtime.cache().cache_put(
        root.path(),
        RecordKind::Dirents,
        None,
        &CacheRecord::new(RecordKind::Dirents, payload),
    );

    let listing = listing(&t, &root, None, &ctx).await;
    for name in [".gitignore", ".ignore", ".rgignore"] {
        let matching: Vec<_> = listing.entries.iter().filter(|e| e.name == name).collect();
        assert_eq!(
            matching.len(),
            1,
            "cached root listing must contain one {name}"
        );
        assert!(matching[0].is_synthetic(), "{name} must be synthetic");
    }

    for _ in 0..2 {
        let node = t.tree.resolve(&path("/.gitignore"), &ctx).await.unwrap();
        assert!(node.is_file() && node.is_synthetic());
    }
}

/// The host reservation is root-scoped: a provider-shaped child below the
/// mount root remains a provider directory and is not intercepted.
#[tokio::test(flavor = "multi_thread")]
async fn root_ignore_names_below_root_remain_provider_owned() {
    let t = test_tree();
    let ctx = RequestCtx::default();
    let hello = t.tree.resolve(&path("/hello"), &ctx).await.unwrap();
    let payload = DirentsPayload {
        entries: vec![DirentRecord {
            name: ".gitignore".to_string(),
            meta: EntryMeta::directory(),
        }],
        exhaustive: true,
        validator: None,
        next_cursor: None,
        paginated: false,
    }
    .serialize()
    .expect("serialize nested dirents");
    t.runtime.cache().cache_put(
        hello.path(),
        RecordKind::Dirents,
        None,
        &CacheRecord::new(RecordKind::Dirents, payload),
    );

    let node = t
        .tree
        .resolve(&path("/hello/.gitignore"), &ctx)
        .await
        .unwrap();
    assert!(node.is_dir());
    assert!(!node.is_synthetic());
}

// --- Serve-cached listing ----------------------------------------------------

/// A warm first-page listing serves the accumulated dirents from cache rather
/// than re-rendering: the same provider entries, the same synthetic controls,
/// and the same resume cursor come back. This is the serve-cached path the
/// provider-returned `Unchanged` branch shares (both route through
/// `listing_from_dirents`); the test provider never returns `Unchanged` for a
/// listing, so the warm-cache hit is the kernel-free face of that behavior.
#[tokio::test(flavor = "multi_thread")]
async fn list_unchanged_serves_cached() {
    let t = test_tree();
    let ctx = RequestCtx;
    let feed = t.tree.resolve(&path("/hello/feed"), &ctx).await.unwrap();

    // Cold: caches the paginated (authoritative) dirents.
    let cold = listing(&t, &feed, None, &ctx).await;
    assert_eq!(provider_names(&cold), ["item-0", "item-1"]);
    assert!(
        cached_dirents(&t.runtime, "/hello/feed").is_some(),
        "a paged listing caches authoritative dirents"
    );

    // Warm: served from the cached authoritative record, same shape.
    let warm = listing(&t, &feed, None, &ctx).await;
    assert_eq!(provider_names(&warm), ["item-0", "item-1"]);
    let mut control_names = synthetic_names(&warm);
    control_names.sort_unstable();
    assert_eq!(
        control_names,
        ["@all", "@next"],
        "the served-cached listing re-presents the controls"
    );
    assert_eq!(warm.next_cursor, Some(Cursor(CachedCursor::Page(1))));
}

// --- Negative cache ----------------------------------------------------------

/// A resolve of a missing child surfaces NotFound and writes a negative index
/// entry, so a second resolve short-circuits to NotFound from the cache (the
/// negative oracle `Namespace::lookup_child` consults before any provider round
/// trip).
#[tokio::test(flavor = "multi_thread")]
async fn lookup_negative_cached() {
    let t = test_tree();
    let ctx = RequestCtx;
    let missing = "/hello/definitely-not-here";

    let err = t
        .tree
        .resolve(&path(missing), &ctx)
        .await
        .expect_err("a missing child resolves NotFound");
    assert_eq!(err.kind, TreeErrorKind::NotFound);

    // The miss armed the live negative index.
    assert!(
        t.runtime
            .cache()
            .negative_for(&path(missing), now_millis())
            .is_some(),
        "a lookup miss must arm the negative index"
    );

    // A second resolve is still NotFound, served from the negative cache.
    let again = t
        .tree
        .resolve(&path(missing), &ctx)
        .await
        .expect_err("the cached negative keeps the child NotFound");
    assert_eq!(again.kind, TreeErrorKind::NotFound);
}
