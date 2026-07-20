//! Exhaustive pagination controls through the production namespace surface.

#![cfg(not(target_os = "wasi"))]

use omnifs_core::path::Path;
use omnifs_engine::{DirCursor, LookupAnswer, Namespace};
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

async fn list(harness: &RuntimeHarness, feed: &LookupAnswer) -> Vec<String> {
    harness
        .namespace
        .readdir(feed.path.clone(), DirCursor::start(), 0)
        .await
        .unwrap()
        .entries
        .into_iter()
        .map(|entry| entry.name)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn reading_next_drains_feed_and_drops_controls() {
    let harness = RuntimeHarness::new(omnifs_itest::TEST_PROVIDER_CONFIG).unwrap();
    let feed = resolve(&harness, "/test/hello/feed").await;
    let first = list(&harness, &feed).await;
    assert!(first.contains(&"@next".to_string()));
    assert!(first.contains(&"@all".to_string()));
    for name in ["@next", "@next"] {
        let control = resolve(&harness, &format!("/test/hello/feed/{name}")).await;
        harness
            .namespace
            .read(control.path, 0, u32::MAX)
            .await
            .unwrap();
    }
    let final_names = list(&harness, &feed).await;
    for name in ["item-0", "item-1", "item-2", "item-3", "item-4", "item-5"] {
        assert_eq!(
            final_names
                .iter()
                .filter(|candidate| candidate.as_str() == name)
                .count(),
            1
        );
    }
    assert!(!final_names.contains(&"@next".to_string()));
    assert!(!final_names.contains(&"@all".to_string()));
}

#[tokio::test(flavor = "multi_thread")]
async fn reading_all_materializes_the_complete_set() {
    let harness = RuntimeHarness::new(omnifs_itest::TEST_PROVIDER_CONFIG).unwrap();
    let feed = resolve(&harness, "/test/hello/feed").await;
    let first = list(&harness, &feed).await;
    assert!(first.contains(&"@all".to_string()));
    let all = resolve(&harness, "/test/hello/feed/@all").await;
    let status = harness.namespace.read(all.path, 0, u32::MAX).await.unwrap();
    assert!(
        String::from_utf8(status.bytes)
            .unwrap()
            .contains("complete")
    );
    let complete = list(&harness, &feed).await;
    for name in ["item-0", "item-1", "item-2", "item-3", "item-4", "item-5"] {
        assert!(complete.contains(&name.to_string()));
    }
    assert!(!complete.contains(&"@next".to_string()));
    assert!(!complete.contains(&"@all".to_string()));
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_snapshot_controls_resolve_after_exhaustion() {
    let harness = RuntimeHarness::new(omnifs_itest::TEST_PROVIDER_CONFIG).unwrap();
    let feed = resolve(&harness, "/test/hello/feed").await;
    let stale = list(&harness, &feed).await;
    assert!(stale.contains(&"@next".to_string()));
    assert!(stale.contains(&"@all".to_string()));
    let all = resolve(&harness, "/test/hello/feed/@all").await;
    harness.namespace.read(all.path, 0, u32::MAX).await.unwrap();
    let fresh = list(&harness, &feed).await;
    assert!(!fresh.contains(&"@next".to_string()));
    assert!(!fresh.contains(&"@all".to_string()));
    for name in ["@next", "@all"] {
        let node = resolve(&harness, &format!("/test/hello/feed/{name}")).await;
        let status = harness
            .namespace
            .read(node.path, 0, u32::MAX)
            .await
            .unwrap();
        assert_eq!(String::from_utf8(status.bytes).unwrap(), "no more pages\n");
    }
}
