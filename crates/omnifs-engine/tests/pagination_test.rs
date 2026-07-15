//! Host-synthesized pagination: the `@next`/`@all` control action.
//!
//! These tests drive `Engine` directly (the FUSE layer synthesizes
//! the `@next`/`@all` directory entries from the cursor these tests assert on,
//! and serves their `read` by calling the same `paginate_*` methods exercised
//! here). The test-provider's `/hello/feed` route yields two `item-*` dir
//! entries per page across three pages: pages 0 and 1 carry a resume cursor,
//! page 2 is terminal.

use omnifs_core::path::Path;
use omnifs_engine::test_support::cache::RecordKind;
use omnifs_engine::test_support::pagination::NextPageOutcome;
use omnifs_engine::view::{CachedCursor, DirentsPayload};
use omnifs_engine::{DirCursor, Namespace};
use omnifs_itest::{RuntimeHarness, make_initialized_runtime};

const CONFIG: &str = r#"
{
    "provider": "test_provider.wasm",
    "mount": "test"
}
"#;

fn p(value: &str) -> Path {
    Path::parse(value).unwrap()
}

/// All cached dirent names, including any synthetic `@`-prefixed controls.
fn cached_dirents(harness: &RuntimeHarness, path: &str) -> DirentsPayload {
    let record = harness
        .runtime
        .resources
        .cache_get(&p(path), RecordKind::Dirents, None)
        .expect("dirents must be cached");
    DirentsPayload::deserialize(&record.payload).expect("dirents must decode")
}

/// All cached dirent names, including any synthetic `@`-prefixed controls.
fn cached_dirent_names(harness: &RuntimeHarness, path: &str) -> Vec<String> {
    let dirents = cached_dirents(harness, path);
    dirents.entries.into_iter().map(|e| e.name).collect()
}

/// Cached dirent names with the synthetic `@next`/`@all` controls filtered out,
/// i.e. the provider-supplied feed items only.
fn cached_item_names(harness: &RuntimeHarness, path: &str) -> Vec<String> {
    cached_dirent_names(harness, path)
        .into_iter()
        .filter(|n| !omnifs_engine::test_support::pagination::is_reserved_provider_leaf(n))
        .collect()
}

/// True when the accumulated dirents carry the synthetic `@next`/`@all`
/// controls. The host appends them on the first page a directory pages and
/// never strips them back out, even once the resume cursor clears, so a name
/// already resolved from an earlier listing snapshot keeps resolving after
/// exhaustion; a FRESH listing separately stops naming them once the cursor
/// clears (`tree::list::listing_from_dirents`), which this raw-cache helper
/// does not observe.
fn has_control_entries(harness: &RuntimeHarness, path: &str) -> bool {
    let names = cached_dirent_names(harness, path);
    names.iter().any(|n| n == "@next") && names.iter().any(|n| n == "@all")
}

fn cached_cursor(harness: &RuntimeHarness, path: &str) -> Option<CachedCursor> {
    cached_dirents(harness, path).next_cursor
}

#[tokio::test]
async fn first_page_carries_cursor_and_caches_it() {
    let harness = make_initialized_runtime(CONFIG);

    let feed = harness
        .namespace
        .lookup(Path::root(), "test")
        .await
        .unwrap();
    let feed = harness.namespace.lookup(feed.path, "hello").await.unwrap();
    let feed = harness.namespace.lookup(feed.path, "feed").await.unwrap();
    let listing = harness
        .namespace
        .readdir(feed.path, DirCursor::start(), 0)
        .await
        .unwrap();
    let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["item-0", "item-1", "@next", "@all"],
        "page 0 entries and pagination controls"
    );
    assert!(
        listing.next.is_some(),
        "/page 0 must carry the resume cursor that drives @next/@all, got {:?}",
        listing.next
    );

    // The cursor lands on the cached dirents record (where the FUSE layer reads
    // it to synthesize @next/@all and to resume pagination).
    assert_eq!(
        cached_cursor(&harness, "/hello/feed"),
        Some(CachedCursor::Page(1))
    );
}

#[tokio::test]
async fn lookup_sibling_hints_preserve_paged_parent_state() {
    let harness = make_initialized_runtime(CONFIG);

    let feed = harness
        .namespace
        .lookup(Path::root(), "test")
        .await
        .unwrap();
    let feed = harness.namespace.lookup(feed.path, "hello").await.unwrap();
    let feed = harness.namespace.lookup(feed.path, "feed").await.unwrap();
    harness
        .namespace
        .readdir(feed.path.clone(), DirCursor::start(), 0)
        .await
        .unwrap();
    let before = cached_dirents(&harness, "/hello/feed");
    assert_eq!(before.validator.as_deref(), Some("feed-page-0"));
    assert_eq!(before.next_cursor, Some(CachedCursor::Page(1)));
    assert!(before.paginated, "seeded feed is a paginated listing");

    let entry = harness.namespace.lookup(feed.path, "item-0").await.unwrap();
    assert_eq!(entry.path.as_str(), "/test/hello/feed/item-0");

    let after = cached_dirents(&harness, "/hello/feed");
    assert_eq!(
        after.validator, before.validator,
        "non-exhaustive lookup hints preserve the listing validator"
    );
    assert_eq!(
        after.next_cursor, before.next_cursor,
        "non-exhaustive lookup hints preserve the resume cursor"
    );
    assert_eq!(
        after.paginated, before.paginated,
        "non-exhaustive lookup hints preserve the paginated marker"
    );
    let names: Vec<&str> = after
        .entries
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    assert!(
        names.contains(&"item-0") && names.contains(&"item-1"),
        "lookup hints merge without dropping the seeded page; got {names:?}"
    );
}

#[tokio::test]
async fn paginate_next_accumulates_and_advances() {
    let harness = make_initialized_runtime(CONFIG);

    // Seed page 0 into the cache.
    let feed = harness
        .namespace
        .lookup(Path::root(), "test")
        .await
        .unwrap();
    let feed = harness.namespace.lookup(feed.path, "hello").await.unwrap();
    let feed = harness.namespace.lookup(feed.path, "feed").await.unwrap();
    harness
        .namespace
        .readdir(feed.path, DirCursor::start(), 0)
        .await
        .unwrap();
    assert_eq!(
        cached_item_names(&harness, "/hello/feed"),
        vec!["item-0", "item-1"]
    );

    // @next loads page 1 and appends it; the cursor advances to page 2. The
    // accumulated dirents still carry @next/@all because more pages remain.
    match harness.runtime.paginate_next(&p("/hello/feed")).await {
        NextPageOutcome::Loaded { added, more } => {
            assert_eq!(added, 2, "page 1 adds two entries");
            assert!(more, "page 1 still has a successor (page 2)");
        },
        _ => panic!("expected Loaded for page 1"),
    }
    assert_eq!(
        cached_item_names(&harness, "/hello/feed"),
        vec!["item-0", "item-1", "item-2", "item-3"],
        "page 1 entries are appended, not replaced"
    );
    assert!(
        has_control_entries(&harness, "/hello/feed"),
        "controls persist while a cursor remains"
    );
    assert_eq!(
        cached_cursor(&harness, "/hello/feed"),
        Some(CachedCursor::Page(2))
    );

    // @next loads the terminal page 2; the cursor is cleared (feed complete)
    // but the controls persist in the accumulated dirents so a name already
    // resolved from an earlier listing snapshot keeps resolving. A fresh
    // listing separately stops naming them once the cursor clears (exercised
    // at the Tree level in `omnifs-itest`'s `pagination_exhaustive`).
    match harness.runtime.paginate_next(&p("/hello/feed")).await {
        NextPageOutcome::Loaded { added, more } => {
            assert_eq!(added, 2);
            assert!(!more, "page 2 is terminal");
        },
        _ => panic!("expected Loaded for the terminal page"),
    }
    assert_eq!(
        cached_item_names(&harness, "/hello/feed"),
        vec!["item-0", "item-1", "item-2", "item-3", "item-4", "item-5"]
    );
    assert!(
        has_control_entries(&harness, "/hello/feed"),
        "controls persist past exhaustion so a stale-snapshot @next/@all keeps resolving"
    );
    assert_eq!(
        cached_cursor(&harness, "/hello/feed"),
        None,
        "the feed is exhausted: no cursor, so a fresh listing hides @next/@all"
    );

    // A further @next on the exhausted feed reports no more pages.
    assert!(
        matches!(
            harness.runtime.paginate_next(&p("/hello/feed")).await,
            NextPageOutcome::NoMore
        ),
        "exhausted feed yields NoMore"
    );
}

#[tokio::test]
async fn paginate_all_expands_to_completion() {
    let harness = make_initialized_runtime(CONFIG);

    // Seed page 0.
    let feed = harness
        .namespace
        .lookup(Path::root(), "test")
        .await
        .unwrap();
    let feed = harness.namespace.lookup(feed.path, "hello").await.unwrap();
    let feed = harness.namespace.lookup(feed.path, "feed").await.unwrap();
    harness
        .namespace
        .readdir(feed.path, DirCursor::start(), 0)
        .await
        .unwrap();

    // @all loops to exhaustion. Two further pages (1 and 2) remain after the
    // seeded page 0, adding 4 entries total.
    let summary = harness.runtime.paginate_all(&p("/hello/feed")).await;
    assert_eq!(summary, "loaded 2 pages (+4); complete\n");

    assert_eq!(
        cached_item_names(&harness, "/hello/feed"),
        vec!["item-0", "item-1", "item-2", "item-3", "item-4", "item-5"]
    );
    assert!(
        has_control_entries(&harness, "/hello/feed"),
        "a fully-expanded feed keeps its controls so a stale-snapshot @next/@all keeps resolving"
    );
    assert_eq!(cached_cursor(&harness, "/hello/feed"), None);

    // @all on a fully-expanded feed has nothing to do.
    let summary = harness.runtime.paginate_all(&p("/hello/feed")).await;
    assert_eq!(summary, "no more pages\n");
}

#[tokio::test]
async fn paginate_next_on_non_paged_directory_is_no_more() {
    let harness = make_initialized_runtime(CONFIG);

    // `hello/bundle` is a normal (non-paged) directory; it has no cursor.
    let bundle = harness
        .namespace
        .lookup(Path::root(), "test")
        .await
        .unwrap();
    let bundle = harness
        .namespace
        .lookup(bundle.path, "hello")
        .await
        .unwrap();
    let bundle = harness
        .namespace
        .lookup(bundle.path, "bundle")
        .await
        .unwrap();
    harness
        .namespace
        .readdir(bundle.path, DirCursor::start(), 0)
        .await
        .unwrap();

    assert!(
        matches!(
            harness.runtime.paginate_next(&p("/hello/bundle")).await,
            NextPageOutcome::NoMore
        ),
        "a directory with no resume cursor cannot be paged"
    );
}
