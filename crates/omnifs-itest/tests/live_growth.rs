//! Live-file growth through the production `TreeNamespace` surface.

#![cfg(not(target_os = "wasi"))]

use omnifs_core::path::Path;
use omnifs_engine::{EntryKind, LookupAnswer, Namespace, NsEvent, ReadStyle};
use omnifs_itest::RuntimeHarness;

async fn resolve(harness: &RuntimeHarness, value: &str) -> LookupAnswer {
    let namespace = harness.namespace.as_ref();
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
async fn live_file_grows_and_follow_read_observes_appended_bytes() {
    let harness = RuntimeHarness::new(omnifs_itest::TEST_PROVIDER_CONFIG).unwrap();
    let namespace = harness.namespace.as_ref();
    let node = resolve(&harness, "/test/hello/live-log").await;
    assert_eq!(node.attrs.kind, EntryKind::File);
    assert_eq!(node.attrs.read_style, ReadStyle::Ranged);
    let mut events = namespace.subscribe();

    let first = namespace.read(node.path.clone(), 0, 128).await.unwrap();
    assert_eq!(first.bytes.len(), 12);
    assert!(first.eof, "the first observed live extent is a bounded EOF");
    let baseline = first.attrs.size;
    assert!(
        baseline >= 12,
        "initial live read must establish a real baseline"
    );
    let observed = tokio::time::timeout(std::time::Duration::from_secs(4), async {
        let mut last = baseline;
        loop {
            match events.recv().await.expect("live namespace event stream") {
                NsEvent::AttrsChanged { path, attrs } => {
                    assert_eq!(path, node.path);
                    assert!(attrs.size >= last);
                    if attrs.size > baseline {
                        break attrs.size;
                    }
                    last = attrs.size;
                },
                NsEvent::InvalidateSubtree { .. } => {},
            }
        }
    })
    .await
    .expect("live follow pump must publish growth within the bounded timeout");

    let second = namespace.read(node.path.clone(), 12, 128).await.unwrap();
    assert!(
        second.attrs.size >= observed,
        "follow read must retain the extent observed from the growth event"
    );
    assert!(
        second.bytes.len() as u64 >= observed.saturating_sub(12),
        "follow read must cover the extent observed from the growth event"
    );
    assert!(
        second.eof,
        "the second read reaches the newly observed extent"
    );
}
