//! Host-synthesized pagination: the `@next`/`@all` control action.
//!
//! These tests drive `Runtime` directly (the FUSE layer synthesizes
//! the `@next`/`@all` directory entries from the cursor these tests assert on,
//! and serves their `read` by calling the same `paginate_*` methods exercised
//! here). The test-provider's `/hello/feed` route yields two `item-*` dir
//! entries per page across three pages: pages 0 and 1 carry a resume cursor,
//! page 2 is terminal.

use omnifs_cache::RecordKind;
use omnifs_core::view::{CachedCursor, DirentsPayload};
use omnifs_host::LookupOutcome;
use omnifs_host::pagination::NextPageOutcome;
use omnifs_itest::{RuntimeHarness, make_initialized_runtime};
use omnifs_wit::provider::types::{Cursor, ListChildrenResult};

const CONFIG: &str = r#"
{
    "provider": "test_provider.wasm",
    "mount": "test",
    "capabilities": { "domains": ["httpbin.org"] }
}
"#;

/// All cached dirent names, including any synthetic `@`-prefixed controls.
fn cached_dirents(harness: &RuntimeHarness, path: &str) -> DirentsPayload {
    let record = harness
        .runtime
        .cache_get(path, RecordKind::Dirents, None)
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
        .filter(|n| !omnifs_host::pagination::is_reserved_provider_leaf(n))
        .collect()
}

/// True when the accumulated dirents carry the synthetic `@next`/`@all`
/// controls (the host appends them only while a resume cursor remains).
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

    let result = harness
        .runtime
        .namespace()
        .list_children("/hello/feed", None, None, None)
        .await
        .unwrap();

    let ListChildrenResult::Entries(listing) = result else {
        panic!("expected entries, got {result:?}");
    };
    let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["item-0", "item-1"], "page 0 entries");
    assert!(
        matches!(listing.next_cursor, Some(Cursor::Page(1))),
        "/page 0 must carry the resume cursor that drives @next/@all, got {:?}",
        listing.next_cursor
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

    harness
        .runtime
        .namespace()
        .list_children("/hello/feed", None, None, None)
        .await
        .unwrap();
    let before = cached_dirents(&harness, "/hello/feed");
    assert_eq!(before.validator.as_deref(), Some("feed-page-0"));
    assert_eq!(before.next_cursor, Some(CachedCursor::Page(1)));
    assert!(before.paginated, "seeded feed is a paginated listing");

    let lookup = harness
        .runtime
        .namespace()
        .lookup_child("/hello/feed", "item-0", None)
        .await
        .unwrap();
    let LookupOutcome::Entry(entry) = lookup else {
        panic!("expected lookup entry");
    };
    assert_eq!(entry.path().as_str(), "/hello/feed/item-0");

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
    harness
        .runtime
        .namespace()
        .list_children("/hello/feed", None, None, None)
        .await
        .unwrap();
    assert_eq!(
        cached_item_names(&harness, "/hello/feed"),
        vec!["item-0", "item-1"]
    );

    // @next loads page 1 and appends it; the cursor advances to page 2. The
    // accumulated dirents still carry @next/@all because more pages remain.
    match harness.runtime.paginate_next("/hello/feed", None).await {
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
    // and the controls drop out of the accumulated dirents.
    match harness.runtime.paginate_next("/hello/feed", None).await {
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
        !has_control_entries(&harness, "/hello/feed"),
        "controls drop out once the feed is exhausted"
    );
    assert_eq!(
        cached_cursor(&harness, "/hello/feed"),
        None,
        "/the feed is exhausted: no cursor, so FUSE drops @next/@all"
    );

    // A further @next on the exhausted feed reports no more pages.
    assert!(
        matches!(
            harness.runtime.paginate_next("/hello/feed", None).await,
            NextPageOutcome::NoMore
        ),
        "exhausted feed yields NoMore"
    );
}

#[tokio::test]
async fn paginate_all_expands_to_completion() {
    let harness = make_initialized_runtime(CONFIG);

    // Seed page 0.
    harness
        .runtime
        .namespace()
        .list_children("/hello/feed", None, None, None)
        .await
        .unwrap();

    // @all loops to exhaustion. Two further pages (1 and 2) remain after the
    // seeded page 0, adding 4 entries total.
    let summary = harness.runtime.paginate_all("/hello/feed", None).await;
    assert_eq!(summary, "loaded 2 pages (+4); complete\n");

    assert_eq!(
        cached_item_names(&harness, "/hello/feed"),
        vec!["item-0", "item-1", "item-2", "item-3", "item-4", "item-5"]
    );
    assert!(
        !has_control_entries(&harness, "/hello/feed"),
        "a fully-expanded feed has no controls"
    );
    assert_eq!(cached_cursor(&harness, "/hello/feed"), None);

    // @all on a fully-expanded feed has nothing to do.
    let summary = harness.runtime.paginate_all("/hello/feed", None).await;
    assert_eq!(summary, "no more pages\n");
}

#[tokio::test]
async fn paginate_next_on_non_paged_directory_is_no_more() {
    let harness = make_initialized_runtime(CONFIG);

    // `hello/bundle` is a normal (non-paged) directory; it has no cursor.
    harness
        .runtime
        .namespace()
        .list_children("/hello/bundle", None, None, None)
        .await
        .unwrap();

    assert!(
        matches!(
            harness.runtime.paginate_next("/hello/bundle", None).await,
            NextPageOutcome::NoMore
        ),
        "a directory with no resume cursor cannot be paged"
    );
}
