//! Synthetic pagination and mount-root controls through `TreeNamespace`.

#![cfg(not(target_os = "wasi"))]

use omnifs_core::path::Path;
use omnifs_engine::{DirCursor, LookupAnswer, Namespace};
use omnifs_itest::make_runtime;

async fn resolve(namespace: &dyn Namespace, value: &str) -> LookupAnswer {
    let mut answer = LookupAnswer {
        path: Path::root(),
        attrs: namespace.getattr(Path::root()).await.unwrap(),
    };
    for segment in Path::parse(value).unwrap().segments() {
        answer = namespace.lookup(answer.path, segment).await.unwrap();
    }
    answer
}

#[tokio::test(flavor = "multi_thread")]
async fn list_emits_pagination_controls() {
    let harness = make_runtime();
    let namespace = harness.namespace.as_ref();
    let feed = resolve(namespace, "/test/hello/feed").await;
    let page = namespace
        .readdir(feed.path.clone(), DirCursor::start(), 0)
        .await
        .unwrap();
    let names: Vec<&str> = page
        .entries
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    assert!(names.contains(&"@next"));
    assert!(names.contains(&"@all"));
    assert!(page.next.is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn read_next_control_advances_one_page() {
    let harness = make_runtime();
    let namespace = harness.namespace.as_ref();
    let feed = resolve(namespace, "/test/hello/feed").await;
    namespace
        .readdir(feed.path.clone(), DirCursor::start(), 0)
        .await
        .unwrap();
    let next = resolve(namespace, "/test/hello/feed/@next").await;
    let result = namespace.read(next.path, 0, u32::MAX).await.unwrap();
    assert!(
        String::from_utf8(result.bytes)
            .unwrap()
            .contains("+2 entries")
    );
    let listing = namespace
        .readdir(feed.path, DirCursor::start(), 0)
        .await
        .unwrap();
    for name in ["item-0", "item-1", "item-2", "item-3", "item-4", "item-5"] {
        assert!(listing.entries.iter().any(|entry| entry.name == name));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn read_all_control_exhausts_then_control_still_resolves() {
    let harness = make_runtime();
    let namespace = harness.namespace.as_ref();
    let feed = resolve(namespace, "/test/hello/feed").await;
    namespace
        .readdir(feed.path.clone(), DirCursor::start(), 0)
        .await
        .unwrap();
    let all = resolve(namespace, "/test/hello/feed/@all").await;
    let result = namespace.read(all.path, 0, u32::MAX).await.unwrap();
    assert!(
        String::from_utf8(result.bytes)
            .unwrap()
            .contains("complete")
    );
    let listing = namespace
        .readdir(feed.path, DirCursor::start(), 0)
        .await
        .unwrap();
    assert!(listing.next.is_none());
    for name in ["item-0", "item-1", "item-2", "item-3", "item-4", "item-5"] {
        assert!(listing.entries.iter().any(|entry| entry.name == name));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn root_ignore_synthesized() {
    let harness = make_runtime();
    let namespace = harness.namespace.as_ref();
    let mount = resolve(namespace, "/test").await;
    let page = namespace
        .readdir(mount.path.clone(), DirCursor::start(), 0)
        .await
        .unwrap();
    for name in [".gitignore", ".ignore", ".rgignore"] {
        assert_eq!(
            page.entries
                .iter()
                .filter(|entry| entry.name == name)
                .count(),
            1,
            "synthetic ignore name must occur exactly once"
        );
    }
    let gitignore = page
        .entries
        .iter()
        .find(|entry| entry.name == ".gitignore")
        .expect("gitignore entry");
    assert_eq!(gitignore.attrs.kind, omnifs_engine::EntryKind::File);
    assert_eq!(
        namespace
            .read(gitignore.path.clone(), 0, u32::MAX)
            .await
            .unwrap()
            .bytes,
        b"@*\n/README.md\n/*/README.md\n"
    );
    let hello = namespace.lookup(mount.path, "hello").await.unwrap();
    let message = namespace.lookup(hello.path, "message").await.unwrap();
    assert_eq!(
        namespace
            .read(message.path, 0, u32::MAX)
            .await
            .unwrap()
            .bytes,
        b"Hello, world!"
    );
}
