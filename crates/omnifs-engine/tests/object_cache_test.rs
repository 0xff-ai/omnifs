//! Runtime-visible cache behavior, without reaching into durable key layout.

use omnifs_core::path::Path;
use omnifs_engine::test_support::cache::publish_effects_for_test;
use omnifs_engine::{LookupAnswer, Namespace};
use omnifs_itest::make_initialized_runtime;
use omnifs_wit::provider::types::{CanonicalStore, Effects, LogicalId};

const CONFIG: &str = r#"{"provider":"test_provider.wasm","mount":"test"}"#;

async fn resolve(namespace: &dyn Namespace, value: &str) -> LookupAnswer {
    let attrs = namespace.getattr(Path::root()).await.unwrap();
    let mut answer = LookupAnswer::found(Path::root(), attrs);
    for segment in Path::parse(value).unwrap().segments() {
        answer = namespace.lookup(answer.path, segment).await.unwrap();
    }
    answer
}

#[tokio::test(flavor = "multi_thread")]
async fn plain_path_lookup_ignores_unrelated_indexed_validator() {
    let harness = make_initialized_runtime(CONFIG);
    let target = resolve(harness.namespace.as_ref(), "/test/hello/message").await;
    let effects = Effects {
        canonical: vec![CanonicalStore {
            id: LogicalId {
                kind: "unrelated".into(),
                captures: Vec::new(),
            },
            validator: Some("unrelated-v1".into()),
            bytes: b"wrong".to_vec(),
            view_leaves: vec!["/hello/message".into()],
        }],
        fs: Vec::new(),
        invalidations: Vec::new(),
    };
    publish_effects_for_test(
        &harness.runtime,
        &effects,
        harness.runtime.resources.current_epoch(),
    )
    .unwrap();
    let read = harness
        .namespace
        .read(target.path, 0, u32::MAX)
        .await
        .unwrap();
    assert_eq!(read.bytes, b"Hello, world!");
}

#[tokio::test(flavor = "multi_thread")]
async fn negative_lookup_is_a_durable_missing_answer() {
    let harness = make_initialized_runtime(CONFIG);
    let hello = resolve(harness.namespace.as_ref(), "/test/hello").await;
    let first = harness
        .namespace
        .lookup(hello.path.clone(), "definitely-missing")
        .await
        .expect("provider missing lookup answer");
    assert!(first.is_missing());
    harness.runtime.shutdown().unwrap();
    let second = harness
        .namespace
        .lookup(hello.path, "definitely-missing")
        .await
        .expect("durable negative lookup answer");
    assert!(second.is_missing());
}
