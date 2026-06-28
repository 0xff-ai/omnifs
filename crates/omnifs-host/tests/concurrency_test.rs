//! Proves the headline property of the async provider runtime: two independent
//! filesystem operations can be in flight on ONE provider instance at the same
//! time, each suspended on its own host callout.

use std::time::Duration;

use omnifs_core::path::Path;
use omnifs_host::PendingTestCallout;
use omnifs_itest::{make_engine, make_runtime};
use omnifs_wit::provider::types::{ByteSource, CalloutResult, Header, HttpResponse};

fn p(value: &str) -> Path {
    Path::parse(value).unwrap()
}

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
    let engine = make_engine();
    let harness = make_runtime(&engine);
    let runtime = &harness.runtime;

    let namespace = runtime.namespace();
    let path_a = p("/hello/remote-a");
    let path_b = p("/hello/remote-b");
    let read_a = namespace.read_file(&path_a, String::new(), None);
    let read_b = namespace.read_file(&path_b, String::new(), None);

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

    for result in [
        result_a.expect("read a returns"),
        result_b.expect("read b returns"),
    ] {
        let ByteSource::Inline(bytes) = result.bytes else {
            panic!("remote read serves inline bytes, got {:?}", result.bytes);
        };
        assert_eq!(bytes, b"concurrent-ok");
    }
}
