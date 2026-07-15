//! Provider-terminal publication witnesses.

use omnifs_core::path::Path;
use omnifs_engine::test_support::cache::publish_effects_for_test;
use omnifs_engine::{DirCursor, EntryKind, Namespace};
use omnifs_itest::make_initialized_runtime;
use omnifs_wit::provider::types::{
    ByteSource, CanonicalStore, Effects, FileAttrs, FileOut, FileSize, FsKind, FsWrite, IdCapture,
    LogicalId, Stability,
};

const CONFIG: &str = r#"{"provider":"test_provider.wasm","mount":"test"}"#;

fn id() -> LogicalId {
    LogicalId {
        kind: "test.atomic".into(),
        captures: vec![IdCapture {
            name: "id".into(),
            value: "1".into(),
        }],
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn published_effects_atomically_expose_object_file_attrs_and_dirents() {
    let harness = make_initialized_runtime(CONFIG);
    let mount = harness
        .namespace
        .lookup(Path::root(), "test")
        .await
        .unwrap();
    let hello = harness.namespace.lookup(mount.path, "hello").await.unwrap();
    let bundle = harness
        .namespace
        .lookup(hello.path, "bundle")
        .await
        .unwrap();
    let effects = Effects {
        canonical: vec![CanonicalStore {
            id: id(),
            validator: Some("v1".into()),
            bytes: b"file".to_vec(),
            view_leaves: vec!["/hello/bundle/item".into()],
        }],
        fs: vec![
            FsWrite {
                id: None,
                path: "/hello/bundle".into(),
                kind: FsKind::Directory(true),
            },
            FsWrite {
                id: Some(id()),
                path: "/hello/bundle/item".into(),
                kind: FsKind::File(FileOut {
                    content_type: None,
                    attrs: FileAttrs {
                        size: FileSize::Exact(4),
                        stability: Stability::Stable,
                        version_token: Some("attrs-v1".into()),
                    },
                    bytes: ByteSource::Canonical,
                }),
            },
        ],
        invalidations: Vec::new(),
    };
    publish_effects_for_test(
        &harness.runtime,
        &effects,
        harness.runtime.resources.current_epoch(),
    )
    .unwrap();

    let listing = harness
        .namespace
        .readdir(bundle.path.clone(), DirCursor::start(), 0)
        .await
        .unwrap();
    assert!(listing.entries.iter().any(|entry| entry.name == "item"));
    let item = harness.namespace.lookup(bundle.path, "item").await.unwrap();
    assert_eq!(item.attrs.kind, EntryKind::File);
    assert_eq!(item.attrs.size, 4);
    let read = harness
        .namespace
        .read(item.path, 0, u32::MAX)
        .await
        .unwrap();
    assert_eq!(read.bytes, b"file");
}
