//! Snapshot test for callout span instrumentation.
//!
//! Drives canned futures/results through the same outer/inner span
//! structure that `dispatch_one` uses in production, then asserts the
//! captured `fmt` layer output contains the request-side fields on the
//! `new` line and the late-recorded response-side fields on the
//! `close` line. Coverage spans all five real callout kinds plus one
//! unsupported variant.

use omnifs_host::omnifs::provider::types as wit_types;
use omnifs_host::runtime::__test_support::{LogUrl, WitHeaders, kind_label, record_outcome};
use std::io;
use std::sync::{Arc, Mutex};
use tracing::Instrument;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::fmt::format::FmtSpan;

#[derive(Clone, Default)]
struct CapturedWriter {
    buffer: Arc<Mutex<Vec<u8>>>,
}

impl CapturedWriter {
    fn snapshot(&self) -> String {
        let bytes = self.buffer.lock().unwrap().clone();
        String::from_utf8(bytes).expect("captured tracing output is utf-8")
    }
}

impl io::Write for CapturedWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buffer.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for CapturedWriter {
    type Writer = CapturedWriter;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

// --- Canned instrumented executor methods --------------------------------
//
// Each helper mirrors the production `#[instrument]` annotations on
// `HttpExecutor::fetch`, `BlobExecutor::fetch`, `BlobExecutor::read`,
// `GitExecutor::open_repo`, and `ArchiveExecutor::open`. The function
// bodies do not perform real I/O; they synthesize a `CalloutResult` and
// call `record_outcome` so the late-bound span fields land before the
// span closes.

#[tracing::instrument(target = "omnifs_callout", skip_all, fields(
    method = req.method.as_str(),
    url = %LogUrl(&req.url),
    request_headers = %WitHeaders(&req.headers),
    request_body_bytes = req.body.as_ref().map_or(0, Vec::len),
    status = tracing::field::Empty,
    response_headers = tracing::field::Empty,
    response_body_bytes = tracing::field::Empty,
    error.kind = tracing::field::Empty,
    error.message = tracing::field::Empty,
    error.retryable = tracing::field::Empty,
))]
async fn fake_http_fetch(req: &wit_types::HttpRequest) -> wit_types::CalloutResult {
    let result = wit_types::CalloutResult::HttpResponse(wit_types::HttpResponse {
        status: 200,
        headers: vec![
            wit_types::Header {
                name: "Content-Type".into(),
                value: "application/json".into(),
            },
            wit_types::Header {
                name: "Set-Cookie".into(),
                value: "session=abcdef".into(),
            },
        ],
        body: b"{}".to_vec(),
    });
    record_outcome(&result);
    result
}

#[tracing::instrument(target = "omnifs_callout", skip_all, fields(
    cache_key = %req.cache_key,
    method = req.method.as_str(),
    url = %LogUrl(&req.url),
    request_headers = %WitHeaders(&req.headers),
    request_body_bytes = req.body.as_ref().map_or(0, Vec::len),
    blob = tracing::field::Empty,
    status = tracing::field::Empty,
    response_headers = tracing::field::Empty,
    response_body_bytes = tracing::field::Empty,
    error.kind = tracing::field::Empty,
    error.message = tracing::field::Empty,
    error.retryable = tracing::field::Empty,
))]
async fn fake_blob_fetch(req: &wit_types::BlobFetchRequest) -> wit_types::CalloutResult {
    let result = wit_types::CalloutResult::BlobFetched(wit_types::BlobFetched {
        blob: 4242,
        size: 1024,
        content_type: Some("application/octet-stream".into()),
        etag: Some("etag-abc".into()),
        status: 200,
        response_headers: vec![wit_types::Header {
            name: "Content-Type".into(),
            value: "application/octet-stream".into(),
        }],
    });
    record_outcome(&result);
    result
}

#[tracing::instrument(target = "omnifs_callout", skip_all, fields(
    blob = req.blob,
    offset = req.offset,
    len = ?req.len,
    response_body_bytes = tracing::field::Empty,
    error.kind = tracing::field::Empty,
    error.message = tracing::field::Empty,
    error.retryable = tracing::field::Empty,
))]
fn fake_blob_read(req: &wit_types::ReadBlobRequest) -> wit_types::CalloutResult {
    let _ = req;
    let result = wit_types::CalloutResult::BlobRead(b"hello world".to_vec());
    record_outcome(&result);
    result
}

#[tracing::instrument(target = "omnifs_callout", skip_all, fields(
    url = %LogUrl(&req.clone_url),
    tree_ref = tracing::field::Empty,
    error.kind = tracing::field::Empty,
    error.message = tracing::field::Empty,
    error.retryable = tracing::field::Empty,
))]
fn fake_git_open(req: &wit_types::GitOpenRequest) -> wit_types::CalloutResult {
    let _ = req;
    let result =
        wit_types::CalloutResult::GitRepoOpened(wit_types::GitRepoInfo { repo: 7, tree: 7 });
    record_outcome(&result);
    result
}

#[tracing::instrument(target = "omnifs_callout", skip_all, fields(
    blob = req.blob,
    format = ?req.format,
    strip_prefix = req.strip_prefix.as_deref().unwrap_or(""),
    tree_ref = tracing::field::Empty,
    error.kind = tracing::field::Empty,
    error.message = tracing::field::Empty,
    error.retryable = tracing::field::Empty,
))]
async fn fake_archive_open(req: &wit_types::ArchiveOpenRequest) -> wit_types::CalloutResult {
    let _ = req;
    let result = wit_types::CalloutResult::ArchiveOpened(wit_types::ArchiveOpened { tree: 99 });
    record_outcome(&result);
    result
}

/// Dispatch wrapper that mirrors `Callouts::dispatch_one` for an
/// individual canned future. Wraps `fut` in the outer `callout` span
/// carrying `operation_id`, `callout_index`, and `kind`.
async fn dispatch_one<F>(
    op_id: u64,
    index: usize,
    callout: &wit_types::Callout,
    fut: F,
) -> F::Output
where
    F: std::future::Future,
{
    fut.instrument(tracing::info_span!(
        target: "omnifs_callout",
        "callout",
        operation_id = op_id,
        callout_index = index,
        kind = kind_label(callout),
    ))
    .await
}

/// Synchronous variant for callout methods that aren't `async`.
fn dispatch_one_sync<R>(
    op_id: u64,
    index: usize,
    callout: &wit_types::Callout,
    body: impl FnOnce() -> R,
) -> R {
    let span = tracing::info_span!(
        target: "omnifs_callout",
        "callout",
        operation_id = op_id,
        callout_index = index,
        kind = kind_label(callout),
    );
    let _entered = span.enter();
    body()
}

fn http_request() -> wit_types::HttpRequest {
    wit_types::HttpRequest {
        method: "GET".to_string(),
        url: "https://api.example.com/items?access_token=secret&page=2".to_string(),
        headers: vec![
            wit_types::Header {
                name: "User-Agent".into(),
                value: "omnifs-test".into(),
            },
            wit_types::Header {
                name: "Authorization".into(),
                value: "Bearer should-not-leak".into(),
            },
        ],
        body: None,
    }
}

fn blob_fetch_request() -> wit_types::BlobFetchRequest {
    wit_types::BlobFetchRequest {
        method: "GET".to_string(),
        url: "https://cdn.example.com/pkg-1.0.crate".to_string(),
        headers: vec![wit_types::Header {
            name: "Authorization".into(),
            value: "Bearer should-not-leak".into(),
        }],
        body: None,
        cache_key: "pkg/pkg-1.0.crate".to_string(),
    }
}

fn read_blob_request() -> wit_types::ReadBlobRequest {
    wit_types::ReadBlobRequest {
        blob: 4242,
        offset: 0,
        len: Some(64),
    }
}

fn git_open_request() -> wit_types::GitOpenRequest {
    wit_types::GitOpenRequest {
        clone_url: "https://user:pass@github.com/example/repo.git".to_string(),
        cache_key: "github.com/example/repo".to_string(),
    }
}

fn archive_open_request() -> wit_types::ArchiveOpenRequest {
    wit_types::ArchiveOpenRequest {
        blob: 4242,
        format: wit_types::ArchiveFormat::TarGz,
        strip_prefix: Some("pkg-1.0/".to_string()),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn http_fetch_span_records_request_and_response_fields() {
    let req = http_request();
    let callout = wit_types::Callout::Fetch(req.clone());
    let output = run_with_capture_async(|| async move {
        let _ = dispatch_one(11, 0, &callout, fake_http_fetch(&req)).await;
    })
    .await;

    assert_contains_new_line(
        &output,
        &[
            "kind=\"http.fetch\"",
            "method=\"GET\"",
            "url=",
            "request_headers=",
        ],
    );
    assert_contains_close_line(
        &output,
        &["status=200", "response_headers=", "response_body_bytes=2"],
    );
    assert_present_on_new_and_close(&output, "operation_id=11");
    assert_present_on_new_and_close(&output, "callout_index=0");
    assert!(
        output.contains("access_token=redacted"),
        "url should redact access_token: {output}"
    );
    assert!(
        output.contains("Authorization=<redacted>"),
        "auth header must be redacted: {output}"
    );
    assert!(
        output.contains("Set-Cookie=<redacted>"),
        "response cookie must be redacted: {output}"
    );
    assert!(!output.contains("should-not-leak"));
    assert!(!output.contains("session=abcdef"));
}

#[tokio::test(flavor = "current_thread")]
async fn blob_fetch_span_records_cache_key_and_late_bound_blob() {
    let req = blob_fetch_request();
    let callout = wit_types::Callout::FetchBlob(req.clone());
    let output = run_with_capture_async(|| async move {
        let _ = dispatch_one(12, 1, &callout, fake_blob_fetch(&req)).await;
    })
    .await;

    assert_contains_new_line(
        &output,
        &[
            "kind=\"blob.fetch\"",
            "cache_key=pkg/pkg-1.0.crate",
            "method=\"GET\"",
            "url=",
        ],
    );
    assert_contains_close_line(
        &output,
        &[
            "blob=4242",
            "status=200",
            "response_headers=",
            "response_body_bytes=1024",
        ],
    );
    assert_present_on_new_and_close(&output, "operation_id=12");
    assert_present_on_new_and_close(&output, "callout_index=1");
    assert!(output.contains("Authorization=<redacted>"));
    assert!(!output.contains("should-not-leak"));
}

#[tokio::test(flavor = "current_thread")]
async fn blob_read_span_records_response_body_bytes_at_close() {
    let req = read_blob_request();
    let callout = wit_types::Callout::ReadBlob(req.clone());
    let output = run_with_capture_async(|| async move {
        dispatch_one_sync(13, 2, &callout, || fake_blob_read(&req));
    })
    .await;

    assert_contains_new_line(
        &output,
        &["kind=\"blob.read\"", "blob=4242", "offset=0", "len="],
    );
    assert_contains_close_line(&output, &["response_body_bytes=11"]);
    assert_present_on_new_and_close(&output, "operation_id=13");
    assert_present_on_new_and_close(&output, "callout_index=2");
}

#[tokio::test(flavor = "current_thread")]
async fn git_open_span_records_tree_ref_at_close() {
    let req = git_open_request();
    let callout = wit_types::Callout::GitOpenRepo(req.clone());
    let output = run_with_capture_async(|| async move {
        dispatch_one_sync(14, 3, &callout, || fake_git_open(&req));
    })
    .await;

    assert_contains_new_line(&output, &["kind=\"git.open_repo\"", "url="]);
    assert_contains_close_line(&output, &["tree_ref=7"]);
    assert_present_on_new_and_close(&output, "operation_id=14");
    assert_present_on_new_and_close(&output, "callout_index=3");
    // The URL must redact userinfo.
    assert!(
        !output.contains("user:pass"),
        "git URL must strip userinfo: {output}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn archive_open_span_records_tree_ref_at_close() {
    let req = archive_open_request();
    let callout = wit_types::Callout::OpenArchive(req.clone());
    let output = run_with_capture_async(|| async move {
        let _ = dispatch_one(15, 4, &callout, fake_archive_open(&req)).await;
    })
    .await;

    assert_contains_new_line(
        &output,
        &[
            "kind=\"archive.open\"",
            "blob=4242",
            "format=",
            "strip_prefix=\"pkg-1.0/\"",
        ],
    );
    assert_contains_close_line(&output, &["tree_ref=99"]);
    assert_present_on_new_and_close(&output, "operation_id=15");
    assert_present_on_new_and_close(&output, "callout_index=4");
}

#[tokio::test(flavor = "current_thread")]
async fn unsupported_callout_emits_kind_unsupported_and_returns_callout_error() {
    let callout = wit_types::Callout::StreamOpen(wit_types::HttpRequest {
        method: "GET".to_string(),
        url: "https://example.com/stream".to_string(),
        headers: Vec::new(),
        body: None,
    });

    let mut returned: Option<wit_types::CalloutResult> = None;
    let returned_ref = &mut returned;
    let output = run_with_capture_async(|| async move {
        let result = dispatch_one(16, 5, &callout, async {
            // Mirror `Callouts::run_callout`'s unsupported fallback: produce
            // a typed internal CalloutError without an inner executor span.
            wit_types::CalloutResult::CalloutError(wit_types::CalloutError {
                kind: wit_types::ErrorKind::Internal,
                message: "callout type not yet implemented".to_string(),
                retryable: false,
            })
        })
        .await;
        *returned_ref = Some(result);
    })
    .await;

    assert!(
        output.contains("kind=\"unsupported\""),
        "unsupported span must carry kind=unsupported: {output}"
    );
    assert!(output.contains("operation_id=16"));
    assert!(output.contains("callout_index=5"));
    match returned.expect("returned a result") {
        wit_types::CalloutResult::CalloutError(err) => {
            assert!(matches!(err.kind, wit_types::ErrorKind::Internal));
            assert!(!err.retryable);
        },
        other => panic!("expected CalloutError for unsupported, got {other:?}"),
    }
}

// --- helpers --------------------------------------------------------------

async fn run_with_capture_async<F, Fut>(body: F) -> String
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    use tracing::instrument::WithSubscriber;

    let writer = CapturedWriter::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(writer.clone())
        .with_env_filter(tracing_subscriber::EnvFilter::new("omnifs_callout=info"))
        .with_target(false)
        .with_ansi(false)
        .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
        .finish();

    body().with_subscriber(subscriber).await;
    writer.snapshot()
}

fn assert_contains_new_line(output: &str, needles: &[&str]) {
    let line = find_line_with_marker(output, "new")
        .unwrap_or_else(|| panic!("no 'new' span event in output:\n{output}"));
    for needle in needles {
        assert!(
            line.contains(needle),
            "'new' line missing {needle:?}\nline: {line}\nfull: {output}"
        );
    }
}

fn assert_contains_close_line(output: &str, needles: &[&str]) {
    let line = find_line_with_marker(output, "close")
        .unwrap_or_else(|| panic!("no 'close' span event in output:\n{output}"));
    for needle in needles {
        assert!(
            line.contains(needle),
            "'close' line missing {needle:?}\nline: {line}\nfull: {output}"
        );
    }
}

fn assert_present_on_new_and_close(output: &str, needle: &str) {
    let new_line = find_line_with_marker(output, "new")
        .unwrap_or_else(|| panic!("no 'new' event in output:\n{output}"));
    let close_line = find_line_with_marker(output, "close")
        .unwrap_or_else(|| panic!("no 'close' event in output:\n{output}"));
    assert!(
        new_line.contains(needle),
        "new line missing {needle:?}: {new_line}"
    );
    assert!(
        close_line.contains(needle),
        "close line missing {needle:?}: {close_line}"
    );
}

/// `tracing-subscriber`'s formatter prints span lifecycle markers at
/// the end of the prefix as `…<fields>}: new` or `…<fields>}: close
/// time.busy=…`. Find the innermost (deepest, longest prefix) line in
/// `output` whose marker matches — the inner executor span's fields
/// trail the outer `callout` span's fields, so the longest line carries
/// the union of both.
fn find_line_with_marker<'a>(output: &'a str, marker: &str) -> Option<&'a str> {
    let suffix = format!(": {marker}");
    output
        .lines()
        .filter(|line| {
            line.contains(&suffix)
                && match marker {
                    "new" => line.trim_end().ends_with(": new"),
                    "close" => line.contains(": close time."),
                    _ => false,
                }
        })
        .max_by_key(|line| line.len())
}
