//! Public namespace conformance for the in-tree test provider.

#![cfg(not(target_os = "wasi"))]

use omnifs_core::path::Path;
use omnifs_engine::test_support::cache::publish_effects_for_test;
use omnifs_engine::{DirCursor, LookupAnswer, Namespace, NsEvent, ReadStyle};
use omnifs_itest::RuntimeHarness;

async fn resolve(harness: &RuntimeHarness, value: &str) -> LookupAnswer {
    let namespace = harness.namespace.as_ref();
    let attrs = namespace.getattr(Path::root()).await.unwrap();
    let mut answer = LookupAnswer::found(Path::root(), attrs);
    for segment in Path::parse(value).unwrap().segments() {
        answer = namespace.lookup(answer.path, segment).await.unwrap();
    }
    answer
}

async fn names(harness: &RuntimeHarness, value: &str) -> Vec<String> {
    let node = resolve(harness, value).await;
    harness
        .namespace
        .readdir(node.path, DirCursor::start(), 0)
        .await
        .unwrap()
        .entries
        .into_iter()
        .map(|entry| entry.name)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn lists_root_and_nested_directories() {
    let harness = RuntimeHarness::new(omnifs_itest::TEST_PROVIDER_CONFIG).unwrap();
    let root = names(&harness, "/test").await;
    for expected in ["README.md", "items", "hello", "scoped", "dynamic"] {
        assert!(root.iter().any(|name| name == expected));
    }
    let hello = names(&harness, "/test/hello").await;
    for expected in ["message", "greeting", "ranged", "bundle", "feed"] {
        assert!(hello.iter().any(|name| name == expected));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn generated_readmes_are_visible_and_hidden_by_root_ignore_patterns() {
    let harness = RuntimeHarness::new(omnifs_itest::TEST_PROVIDER_CONFIG).unwrap();
    let root = resolve(&harness, "/test/README.md").await;
    let root_bytes = harness
        .namespace
        .read(root.path, 0, u32::MAX)
        .await
        .unwrap()
        .bytes;
    let root_text = String::from_utf8(root_bytes).unwrap();
    assert!(root_text.contains("The keying schema is the path grammar below."));
    let ignore = resolve(&harness, "/test/.gitignore").await;
    assert_eq!(ignore.attrs().unwrap().kind, omnifs_engine::EntryKind::File);
    let bytes = harness
        .namespace
        .read(ignore.path, 0, u32::MAX)
        .await
        .unwrap()
        .bytes;
    assert_eq!(bytes, b"@next\n@all\n/README.md\n/*/README.md\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn lazy_derived_face_returns_the_declared_leaf_bytes() {
    let harness = RuntimeHarness::new(omnifs_itest::TEST_PROVIDER_CONFIG).unwrap();
    let item = resolve(&harness, "/test/items/open/7").await;
    harness
        .namespace
        .readdir(item.path, DirCursor::start(), 0)
        .await
        .unwrap();
    assert_eq!(
        harness
            .namespace
            .read(
                resolve(&harness, "/test/items/open/7/state").await.path,
                0,
                u32::MAX
            )
            .await
            .unwrap()
            .bytes,
        b"open"
    );
    assert_eq!(
        harness
            .namespace
            .read(
                resolve(&harness, "/test/items/open/7/title").await.path,
                0,
                u32::MAX
            )
            .await
            .unwrap()
            .bytes,
        b"Item 7"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn reads_whole_file_exact_bytes() {
    let harness = RuntimeHarness::new(omnifs_itest::TEST_PROVIDER_CONFIG).unwrap();
    let node = resolve(&harness, "/test/hello/message").await;
    let read = harness
        .namespace
        .read(node.path, 0, u32::MAX)
        .await
        .unwrap();
    assert_eq!(read.bytes, b"Hello, world!");
    assert!(read.eof);
    assert_eq!(read.attrs.size, 13);
}

#[tokio::test(flavor = "multi_thread")]
async fn reads_ranged_file_in_chunks() {
    let harness = RuntimeHarness::new(omnifs_itest::TEST_PROVIDER_CONFIG).unwrap();
    let node = resolve(&harness, "/test/hello/ranged").await;
    assert_eq!(node.attrs().unwrap().read_style, ReadStyle::Ranged);
    assert_eq!(
        harness
            .namespace
            .read(node.path.clone(), 2, 4)
            .await
            .unwrap()
            .bytes,
        b"cdef"
    );
    let eof = harness.namespace.read(node.path, 26, 8).await.unwrap();
    assert!(eof.bytes.is_empty());
    assert!(eof.eof);
}

#[tokio::test(flavor = "multi_thread")]
async fn lists_cursored_pages() {
    let harness = RuntimeHarness::new(omnifs_itest::TEST_PROVIDER_CONFIG).unwrap();
    let feed = resolve(&harness, "/test/hello/feed").await;
    let first = harness
        .namespace
        .readdir(feed.path.clone(), DirCursor::start(), 0)
        .await
        .unwrap();
    let warm = harness
        .namespace
        .readdir(feed.path.clone(), DirCursor::start(), 0)
        .await
        .unwrap();
    assert_eq!(
        warm.entries, first.entries,
        "warm listing preserves entries and controls"
    );
    assert_eq!(
        warm.next, first.next,
        "warm listing preserves the resume cursor"
    );
    let second = harness
        .namespace
        .readdir(feed.path, first.next.unwrap(), 0)
        .await
        .unwrap();
    assert!(second.entries.iter().any(|entry| entry.name == "item-2"));
    assert!(second.entries.iter().any(|entry| entry.name == "item-3"));
}

#[tokio::test(flavor = "multi_thread")]
async fn resolves_unrouted_path_as_not_found() {
    let harness = RuntimeHarness::new(omnifs_itest::TEST_PROVIDER_CONFIG).unwrap();
    let parent = resolve(&harness, "/test/hello").await;
    assert!(
        harness
            .namespace
            .lookup(parent.path, "not-routed")
            .await
            .expect("known missing child")
            .is_missing()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn invalidation_evicts_cached_read() {
    let harness = RuntimeHarness::new(omnifs_itest::TEST_PROVIDER_CONFIG).unwrap();
    let node = resolve(&harness, "/test/hello/message").await;
    let before = harness.namespace.getattr(node.path.clone()).await.unwrap();
    let cold = harness
        .namespace
        .read(node.path.clone(), 0, u32::MAX)
        .await
        .unwrap();
    assert_eq!(cold.bytes, b"Hello, world!");
    let mut events = harness.namespace.subscribe();
    publish_effects_for_test(
        &harness.runtime,
        &omnifs_wit::provider::types::Effects {
            canonical: Vec::new(),
            fs: Vec::new(),
            invalidations: vec![omnifs_wit::provider::types::Invalidation::Listing(
                omnifs_wit::provider::types::PathOrPrefix::Path("/hello/message".to_string()),
            )],
        },
        harness.runtime.resources.current_epoch(),
    )
    .unwrap();
    let after = harness.namespace.getattr(node.path.clone()).await.unwrap();
    assert_ne!(after.change, before.change);
    let reread = harness
        .namespace
        .read(node.path.clone(), 0, u32::MAX)
        .await
        .unwrap();
    assert_eq!(reread.bytes, b"Hello, world!");
    assert!(
        matches!(tokio::time::timeout(std::time::Duration::from_secs(2), events.recv()).await.unwrap().unwrap(), NsEvent::InvalidateSubtree { path } if path == node.path)
    );
}
