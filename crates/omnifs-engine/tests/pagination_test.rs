//! Namespace-visible pagination behavior.

use omnifs_core::path::Path;
use omnifs_engine::{DirCursor, Namespace};
use omnifs_itest::{RuntimeHarness, make_initialized_runtime};

const CONFIG: &str = r#"{"provider":"test_provider.wasm","mount":"test"}"#;

async fn feed(harness: &RuntimeHarness) -> omnifs_engine::LookupAnswer {
    let mount = harness
        .namespace
        .lookup(Path::root(), "test")
        .await
        .unwrap();
    let hello = harness.namespace.lookup(mount.path, "hello").await.unwrap();
    harness.namespace.lookup(hello.path, "feed").await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn first_page_carries_cursor_and_manual_cursor_expands_to_completion() {
    let harness = make_initialized_runtime(CONFIG);
    let feed = feed(&harness).await;
    let first = harness
        .namespace
        .readdir(feed.path.clone(), DirCursor::start(), 0)
        .await
        .unwrap();
    assert!(first.next.is_some());
    assert!(first.entries.iter().any(|entry| entry.name == "@next"));

    let mut cursor = first.next.expect("first page cursor");
    let mut names: Vec<_> = first
        .entries
        .into_iter()
        .filter(|entry| !entry.name.starts_with('@'))
        .map(|entry| entry.name)
        .collect();
    loop {
        let page = harness
            .namespace
            .readdir(feed.path.clone(), cursor, 0)
            .await
            .unwrap();
        names.extend(
            page.entries
                .into_iter()
                .filter(|entry| !entry.name.starts_with('@'))
                .map(|entry| entry.name),
        );
        let Some(next) = page.next else { break };
        cursor = next;
    }
    for name in ["item-0", "item-1", "item-2", "item-3", "item-4", "item-5"] {
        assert!(names.iter().any(|candidate| candidate == name));
    }
    assert_eq!(names.len(), 6);
    let sibling = harness.namespace.lookup(feed.path, "item-0").await.unwrap();
    assert_eq!(sibling.attrs.kind, omnifs_engine::EntryKind::Directory);
}

#[tokio::test(flavor = "multi_thread")]
async fn pagination_preserves_siblings_and_non_paged_directories_have_no_next() {
    let harness = make_initialized_runtime(CONFIG);
    let feed = feed(&harness).await;
    let page = harness
        .namespace
        .readdir(feed.path.clone(), DirCursor::start(), 0)
        .await
        .unwrap();
    assert!(page.entries.iter().any(|entry| entry.name == "item-0"));
    assert!(page.entries.iter().any(|entry| entry.name == "@next"));
    let next = page.next.expect("first page cursor");
    let page = harness
        .namespace
        .readdir(feed.path.clone(), next, 0)
        .await
        .unwrap();
    let next = page.next.expect("second page cursor");
    harness
        .namespace
        .readdir(feed.path.clone(), next, 0)
        .await
        .unwrap();
    let sibling = harness
        .namespace
        .lookup(feed.path.clone(), "item-0")
        .await
        .unwrap();
    assert_eq!(sibling.attrs.kind, omnifs_engine::EntryKind::Directory);

    let hello = harness
        .namespace
        .lookup(Path::root(), "test")
        .await
        .unwrap();
    let hello = harness.namespace.lookup(hello.path, "hello").await.unwrap();
    let listing = harness
        .namespace
        .readdir(hello.path, DirCursor::start(), 0)
        .await
        .unwrap();
    assert!(listing.next.is_none());
}
