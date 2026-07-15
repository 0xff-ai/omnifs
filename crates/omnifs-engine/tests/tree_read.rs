//! Read-policy regressions through the public `TreeNamespace` owner.

#![cfg(not(target_os = "wasi"))]

use omnifs_core::path::Path;
use omnifs_engine::test_support::cache::{CanonicalBatchEntry, Record, RecordKind};
use omnifs_engine::view::FilePayload;
use omnifs_engine::{DirCursor, LookupAnswer, Namespace};
use omnifs_itest::make_runtime;

fn path(value: &str) -> Path {
    Path::parse(value).expect("valid test path")
}

async fn resolve(namespace: &dyn Namespace, value: &str) -> LookupAnswer {
    let mut answer = LookupAnswer {
        path: Path::root(),
        attrs: namespace.getattr(Path::root()).await.unwrap(),
    };
    for segment in path(value).segments() {
        let parent = answer.path.clone();
        answer = namespace
            .lookup(parent.clone(), segment)
            .await
            .unwrap_or_else(|error| {
                panic!("lookup {segment:?} under {parent} while resolving {value}: {error:?}")
            });
    }
    answer
}

#[tokio::test(flavor = "multi_thread")]
async fn read_whole_file_second_read_hits_cache() {
    let harness = make_runtime();
    let namespace = harness.namespace.as_ref();
    let node = resolve(namespace, "/test/hello/lazy").await;
    assert_eq!(
        namespace
            .read(node.path.clone(), 0, u32::MAX)
            .await
            .unwrap()
            .bytes,
        b"lazy\n"
    );

    let payload = FilePayload::new(None, b"hit!\n".to_vec())
        .serialize()
        .expect("serialize exact-size cache sentinel");
    harness.runtime.resources.cache_put(
        &path("/hello/lazy"),
        RecordKind::File,
        None,
        &Record::new(RecordKind::File, payload),
    );
    let second = namespace
        .read(node.path.clone(), 0, u32::MAX)
        .await
        .unwrap();
    assert_eq!(second.bytes, b"hit!\n");
    assert!(second.eof);
}

#[tokio::test(flavor = "multi_thread")]
async fn preloaded_empty_file_returns_eof_without_host_callout() {
    let harness = make_runtime();
    let namespace = harness.namespace.as_ref();
    let hello = resolve(namespace, "/test/hello").await;
    namespace
        .readdir(hello.path, DirCursor::start(), 0)
        .await
        .expect("hello listing should publish the preloaded bundle shape");
    let node = resolve(namespace, "/test/hello/bundle/empty").await;
    let answer = namespace
        .read(node.path.clone(), 0, u32::MAX)
        .await
        .unwrap();
    assert!(answer.bytes.is_empty());
    assert!(answer.eof);
    assert!(harness.runtime.try_recv_test_callout().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn read_item_md_is_durably_cached() {
    let harness = make_runtime();
    let namespace = harness.namespace.as_ref();
    namespace
        .readdir(path("/test/items/open"), DirCursor::start(), 0)
        .await
        .unwrap();
    assert!(
        harness
            .runtime
            .resources
            .cached_canonical_for(&path("/items/open/7/item.md"))
            .is_some(),
        "the collection listing must publish the canonical object before invalidation"
    );
    harness
        .runtime
        .resources
        .delete_listing_path(&path("/items/open/7/item.md"));
    let item = resolve(namespace, "/test/items/open/7/item.md").await;
    assert!(
        harness
            .runtime
            .resources
            .cached_canonical_for(&path("/items/open/7/item.md"))
            .is_some(),
        "the collection listing must publish the canonical object before invalidation"
    );
    assert!(
        harness
            .runtime
            .resources
            .cached_canonical_for(&path("/items/open/7/item.md"))
            .is_some(),
        "listing invalidation must preserve the canonical object index"
    );
    assert!(
        harness
            .runtime
            .resources
            .cache_get(&path("/items/open/7/item.md"), RecordKind::File, None)
            .is_none()
    );
    let read = namespace.read(item.path, 0, u32::MAX).await.unwrap();
    assert_eq!(read.bytes, b"# Item 7\n\nBody 7\n");
    assert_eq!(read.attrs.stability, omnifs_engine::StabilityClass::Stable);
    let cached = harness
        .runtime
        .resources
        .cache_get(&path("/items/open/7/item.md"), RecordKind::File, None)
        .expect("cold namespace read publishes the rendered file");
    assert_eq!(
        FilePayload::deserialize(&cached.payload).unwrap().content,
        read.bytes
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn canonical_identity_read_revalidates_without_copying_into_view_cache() {
    let harness = make_runtime();
    let namespace = harness.namespace.as_ref();
    let collection = resolve(namespace, "/test/items/open").await;
    namespace
        .readdir(collection.path, DirCursor::start(), 0)
        .await
        .unwrap();
    let item_path = path("/items/open/7/item.json");
    let item = resolve(namespace, "/test/items/open/7/item.json").await;
    let canonical = harness
        .runtime
        .resources
        .cached_canonical_for(&item_path)
        .unwrap();
    assert_eq!(canonical.validator.as_deref(), Some("item-7-v1"));
    let generation = harness.runtime.resources.current_generation();
    assert!(
        harness
            .runtime
            .resources
            .cache_view_leaf(&item_path, &[], Some(0), generation)
            .unwrap()
    );
    assert!(harness.runtime.resources.view_expired(&item_path, 1));
    let read = namespace.read(item.path, 0, u32::MAX).await.unwrap();
    assert_eq!(
        read.bytes,
        br#"{"number":7,"title":"Item 7","body":"Body 7","state":"open"}"#
    );
    assert!(!harness.runtime.resources.view_expired(&item_path, u64::MAX));
    assert!(
        harness
            .runtime
            .resources
            .cache_get(&item_path, RecordKind::File, None)
            .is_none()
    );
    harness
        .runtime
        .resources
        .put_canonical_batch(
            vec![CanonicalBatchEntry {
                id: canonical.id,
                bytes: canonical.bytes,
                validator: Some("item-7-v0".to_string()),
                view_leaves: vec![item_path.clone()],
            }],
            generation,
        )
        .unwrap();
    assert!(
        harness
            .runtime
            .resources
            .cache_view_leaf(&item_path, &[], Some(0), generation)
            .unwrap()
    );
    let item = resolve(namespace, "/test/items/open/7/item.json").await;
    namespace.read(item.path, 0, u32::MAX).await.unwrap();
    assert_eq!(
        harness
            .runtime
            .resources
            .cached_canonical_for(&item_path)
            .and_then(|canonical| canonical.validator),
        Some("item-7-v1".to_string())
    );
    assert!(
        harness
            .runtime
            .resources
            .cache_get(&item_path, RecordKind::File, None)
            .is_none()
    );
}
