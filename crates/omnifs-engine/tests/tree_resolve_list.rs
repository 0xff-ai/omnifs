//! Namespace path identity and complete mount-root listing coverage.

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
async fn list_root_yields_known_children() {
    let harness = make_runtime();
    let namespace = harness.namespace.as_ref();
    let mount = resolve(namespace, "/test").await;
    let page = namespace
        .readdir(mount.path.clone(), DirCursor::start(), 0)
        .await
        .unwrap();
    let mut names: Vec<&str> = page
        .entries
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    names.sort_unstable();
    assert_eq!(names.len(), 9);
    assert!(names.contains(&"README.md"));
    assert!(names.contains(&"hello"));
    assert!(names.contains(&".gitignore"));
    assert!(names.contains(&".ignore"));
    assert!(names.contains(&".rgignore"));
}

#[tokio::test(flavor = "multi_thread")]
async fn list_hello_yields_representative_children() {
    let harness = make_runtime();
    let namespace = harness.namespace.as_ref();
    let hello = resolve(namespace, "/test/hello").await;
    let page = namespace
        .readdir(hello.path.clone(), DirCursor::start(), 0)
        .await
        .unwrap();
    let names: Vec<&str> = page
        .entries
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    for name in ["README.md", "message", "live-log", "remote-a", "remote-b"] {
        assert!(names.contains(&name), "missing {name} from {names:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn resolve_rehydrates_from_path_with_stable_identity() {
    let harness = make_runtime();
    let namespace = harness.namespace.as_ref();
    let first = resolve(namespace, "/test/hello/message").await;
    let second = namespace.getattr(first.path.clone()).await.unwrap();
    assert_eq!(first.path, Path::parse("/test/hello/message").unwrap());
    assert_eq!(second.kind, first.attrs.kind);
}
