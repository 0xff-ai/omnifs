//! Read-policy regressions through the public `TreeNamespace` owner.

#![cfg(not(target_os = "wasi"))]

use omnifs_core::path::Path;
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

    harness.runtime.shutdown().unwrap();
    let second = namespace
        .read(node.path.clone(), 0, u32::MAX)
        .await
        .unwrap();
    assert_eq!(second.bytes, b"lazy\n");
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
async fn read_item_md_returns_stable_representation() {
    let harness = make_runtime();
    let namespace = harness.namespace.as_ref();
    namespace
        .readdir(path("/test/items/open"), DirCursor::start(), 0)
        .await
        .unwrap();
    let item = resolve(namespace, "/test/items/open/7/item.md").await;
    let read = namespace.read(item.path, 0, u32::MAX).await.unwrap();
    assert_eq!(read.bytes, b"# Item 7\n\nBody 7\n");
    assert_eq!(read.attrs.stability, omnifs_engine::StabilityClass::Stable);
}

#[tokio::test(flavor = "multi_thread")]
async fn canonical_read_repeats_with_stable_bytes() {
    let harness = make_runtime();
    let namespace = harness.namespace.as_ref();
    let collection = resolve(namespace, "/test/items/open").await;
    namespace
        .readdir(collection.path, DirCursor::start(), 0)
        .await
        .unwrap();
    let item = resolve(namespace, "/test/items/open/7/item.json").await;
    let first = namespace
        .read(item.path.clone(), 0, u32::MAX)
        .await
        .unwrap();
    harness.runtime.shutdown().unwrap();
    let second = namespace.read(item.path, 0, u32::MAX).await.unwrap();
    assert_eq!(first.bytes, second.bytes);
    assert_eq!(first.attrs.size, second.attrs.size);
}
