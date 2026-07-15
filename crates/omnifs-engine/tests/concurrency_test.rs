//! Proves the headline property of the async provider runtime: two independent
//! filesystem operations can be in flight on ONE provider instance at the same
//! time, each suspended on its own host callout.

use std::time::Duration;

use omnifs_engine::Namespace;
use omnifs_engine::test_support::PendingTestCallout;
use omnifs_itest::make_runtime;
use omnifs_wit::provider::types::{CalloutResult, Header, HttpResponse};

fn http_ok(body: &[u8]) -> CalloutResult {
    CalloutResult::HttpResponse(HttpResponse {
        status: 200,
        headers: Vec::<Header>::new(),
        body: body.to_vec(),
    })
}

/// Two reads of distinct `/hello/remote-*` paths (distinct paths so they do not
/// coalesce) each suspend on an HTTP callout. The driver answers neither callout
/// until BOTH have been captured, which can only happen if both guest calls are
/// suspended on the same instance simultaneously. A runtime that serialized ops
/// (ran the second only after the first returned) would never surface the second
/// callout, so the `answer_both` loop would spin forever; the outer timeout
/// turns that into a clear failure instead of a hang.
#[tokio::test]
async fn two_ops_suspend_concurrently_on_one_instance() {
    let harness = make_runtime();
    let runtime = &harness.runtime;
    let namespace = &harness.namespace;
    let read_a = namespace.read(
        omnifs_core::path::Path::parse("/test/hello/remote-a").unwrap(),
        0,
        4096,
    );
    let read_b = namespace.read(
        omnifs_core::path::Path::parse("/test/hello/remote-b").unwrap(),
        0,
        4096,
    );

    let answer_both = async {
        let mut pending: Vec<PendingTestCallout> = Vec::new();
        while pending.len() < 2 {
            match runtime.try_recv_test_callout() {
                Some(callout) => pending.push(callout),
                None => tokio::time::sleep(Duration::from_millis(2)).await,
            }
        }
        assert_ne!(
            pending[0].op_id(),
            pending[1].op_id(),
            "two distinct ops must be suspended on host imports at the same instant"
        );
        for callout in pending {
            callout.answer(http_ok(b"concurrent-ok"));
        }
    };

    let (result_a, result_b, ()) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(read_a, read_b, answer_both)
    })
    .await
    .expect("two ops must interleave on one instance, not serialize (timed out)");
    let result_a = result_a.expect("read a returns");
    let result_b = result_b.expect("read b returns");

    for result in [result_a, result_b] {
        assert_eq!(result.bytes, b"concurrent-ok");
    }
}
