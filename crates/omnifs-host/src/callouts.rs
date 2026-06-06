//! Per-callout dispatch and the tracing surface for provider callouts.
//!
//! # Dispatch
//!
//! `Runtime::dispatch_callouts` walks the suspended callout
//! batch, routes each variant to its executor, and returns the matching
//! `CalloutResult` list to `continuation.resume`. `run_callout`'s match
//! is exhaustive; adding a WIT callout requires adding a real executor arm.
//!
//! # Layering: only the public boundary builds `CalloutResult`
//!
//! Each executor module (this one's dispatch, `http.rs`,
//! `blob.rs`, `git.rs`, `archive.rs`) exposes a public callout entry
//! method that returns `wit_types::CalloutResult`. Everything below
//! that method uses typed `Result<T, ExecutorError>` (`BlobError`,
//! `ArchiveError`, `GitError`, …) and propagates with `?`. The
//! conversion happens at exactly one place per executor: the public
//! method's outermost `match` or `From<ExecutorError> for CalloutResult`.
//!
//! This keeps internal helpers free to use the regular Rust error
//! plumbing and concentrates the `CalloutError { kind, message,
//! retryable }` decisions in one auditable spot per executor.
//!
//! # Tracing surface
//!
//! All callout spans use `target = "omnifs_callout"` so
//! `RUST_LOG=omnifs_callout=info` filters everything in this layer
//! without bringing in unrelated host tracing.
//!
//! Each callout produces two spans:
//!
//! 1. The outer `callout` span entered by `dispatch_one`, carrying
//!    cross-cutting context:
//!    - `operation_id`: the host-allocated u64 for the in-flight op
//!    - `callout_index`: position of this callout in the batch
//!    - `kind`: the `CalloutKind::as_str()` value
//!      (`"http.fetch"`, `"git.open_repo"`, `"blob.fetch"`,
//!      `"archive.open"`, or `"blob.read"`)
//!
//! 2. The inner executor span (`#[tracing::instrument]` on the
//!    public method itself), carrying kind-specific fields. Late-bound
//!    fields are declared as `tracing::field::Empty` and populated by
//!    `record_outcome(&result)` before the method returns. The full
//!    field set per kind is:
//!
//!    | Span | Request-side fields at NEW | Response/error fields at CLOSE |
//!    |---|---|---|
//!    | `HttpStack::fetch` | `method`, `url`, `request_headers`, `request_body_bytes` | `status`, `response_headers`, `response_body_bytes`, `error.{kind,message,retryable}` |
//!    | `BlobExecutor::fetch` | `cache_key`, `method`, `url`, `request_headers`, `request_body_bytes` | `blob`, `status`, `response_headers`, `response_body_bytes`, `error.{kind,message,retryable}` |
//!    | `BlobExecutor::read` | `blob`, `offset`, `len` | `response_body_bytes`, `error.{kind,message,retryable}` |
//!    | `GitExecutor::open_repo` | `url` | `tree_ref`, `error.{kind,message,retryable}` |
//!    | `ArchiveExecutor::open` | `blob`, `format`, `strip_prefix` | `tree_ref`, `error.{kind,message,retryable}` |
//!
//! URL and header fields render through `LogUrl` and `WitHeaders`,
//! which redact credentials and sensitive query/header values lazily
//! at `Display` time.
//!
//! Every field a span ever records via `Span::record` must appear in
//! its `#[instrument(fields(...))]` declaration. `tracing` silently
//! drops `record` calls for unknown fields, so adding a new outcome
//! field requires both: declaring it as `Empty` in `fields(...)` and
//! recording it from `record_outcome`.
//!
//! Span timing is reported by the subscriber's `FmtSpan::NEW |
//! FmtSpan::CLOSE` configuration. There is no manually-recorded
//! `elapsed_us` field; the framework emits it on span close.
//!
//! # Adding a callout
//!
//! 1. Add the WIT `callout` variant and `callout-result` arm.
//! 2. Add a new `CalloutKind` variant + `as_str` mapping in this file.
//! 3. Extend `CalloutKind::of` exhaustively (no wildcard arm).
//! 4. Add the executor public method with
//!    `#[tracing::instrument(target = "omnifs_callout", skip_all,
//!    fields(...))]` listing all fields (use `field::Empty` for
//!    late-bound ones) and calling `record_outcome(&result)` before
//!    return.
//! 5. Add a `run_callout` arm dispatching to it.

use crate::Runtime;
use crate::inspector::InspectorCallout;
use crate::log_redaction::WitHeaders;
use omnifs_wit::provider::types as wit_types;
use tracing::Instrument;

#[derive(Debug, Clone, Copy)]
pub(crate) enum CalloutKind {
    HttpFetch,
    GitOpenRepo,
    BlobFetch,
    OpenArchive,
    ReadBlob,
}

impl CalloutKind {
    pub(crate) fn of(callout: &wit_types::Callout) -> Self {
        match callout {
            wit_types::Callout::Fetch(_) => Self::HttpFetch,
            wit_types::Callout::GitOpenRepo(_) => Self::GitOpenRepo,
            wit_types::Callout::FetchBlob(_) => Self::BlobFetch,
            wit_types::Callout::OpenArchive(_) => Self::OpenArchive,
            wit_types::Callout::ReadBlob(_) => Self::ReadBlob,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::HttpFetch => "http.fetch",
            Self::GitOpenRepo => "git.open_repo",
            Self::BlobFetch => "blob.fetch",
            Self::OpenArchive => "archive.open",
            Self::ReadBlob => "blob.read",
        }
    }
}

pub(crate) fn callout_error(
    kind: wit_types::ErrorKind,
    message: impl Into<String>,
    retryable: bool,
) -> wit_types::CalloutResult {
    wit_types::CalloutResult::CalloutError(wit_types::CalloutError {
        kind,
        message: message.into(),
        retryable,
    })
}

pub(crate) fn callout_internal(message: impl Into<String>) -> wit_types::CalloutResult {
    callout_error(wit_types::ErrorKind::Internal, message, false)
}
pub(crate) fn callout_denied(message: impl Into<String>) -> wit_types::CalloutResult {
    callout_error(wit_types::ErrorKind::Denied, message, false)
}
pub(crate) fn callout_not_found(message: impl Into<String>) -> wit_types::CalloutResult {
    callout_error(wit_types::ErrorKind::NotFound, message, false)
}
pub(crate) fn callout_too_large(message: impl Into<String>) -> wit_types::CalloutResult {
    callout_error(wit_types::ErrorKind::TooLarge, message, false)
}
pub(crate) fn callout_invalid(message: impl Into<String>) -> wit_types::CalloutResult {
    callout_error(wit_types::ErrorKind::InvalidInput, message, false)
}
pub(crate) fn callout_network(message: impl Into<String>) -> wit_types::CalloutResult {
    callout_error(wit_types::ErrorKind::Network, message, true)
}

/// Records outcome-side span fields on `Span::current()` for the active
/// instrumented executor span. Called once per callout method before
/// it returns the `CalloutResult`. Each field touched here must be
/// pre-declared in the corresponding `#[instrument]` `fields(...)` (use
/// `tracing::field::Empty` for late-bound fields); span fields not
/// declared up front are silently dropped.
pub(crate) fn record_outcome(result: &wit_types::CalloutResult) {
    let span = tracing::Span::current();
    match result {
        wit_types::CalloutResult::HttpResponse(r) => {
            span.record("status", r.status);
            span.record(
                "response_headers",
                tracing::field::display(WitHeaders(&r.headers)),
            );
            span.record("response_body_bytes", r.body.len());
        },
        wit_types::CalloutResult::BlobFetched(r) => {
            span.record("blob", r.blob);
            span.record("status", r.status);
            span.record(
                "response_headers",
                tracing::field::display(WitHeaders(&r.response_headers)),
            );
            span.record("response_body_bytes", r.size);
        },
        wit_types::CalloutResult::BlobRead(bytes) => {
            span.record("response_body_bytes", bytes.len());
        },
        wit_types::CalloutResult::GitRepoOpened(r) => {
            span.record("tree_ref", r.tree);
        },
        wit_types::CalloutResult::ArchiveOpened(r) => {
            span.record("tree_ref", r.tree);
        },
        wit_types::CalloutResult::CalloutError(e) => {
            span.record("error.kind", tracing::field::debug(&e.kind));
            span.record("error.message", e.message.as_str());
            span.record("error.retryable", e.retryable);
        },
    }
}

impl Runtime {
    /// Runs every callout concurrently and returns positionally aligned
    /// outcomes. The SDK's `join_all` pops outcomes from a FIFO queue in
    /// yield order, so this ordering is load-bearing.
    pub(super) async fn dispatch_callouts(
        &self,
        operation_id: u64,
        callouts: &[wit_types::Callout],
    ) -> Vec<wit_types::CalloutResult> {
        let futures = callouts
            .iter()
            .enumerate()
            .map(|(index, callout)| self.dispatch_one(operation_id, index, callout));
        futures::future::join_all(futures).await
    }

    async fn dispatch_one(
        &self,
        op_id: u64,
        index: usize,
        callout: &wit_types::Callout,
    ) -> wit_types::CalloutResult {
        let live = InspectorCallout::begin(callout, op_id, index);
        let result = self
            .run_callout(callout, op_id)
            .instrument(tracing::info_span!(
                target: "omnifs_callout",
                "callout",
                operation_id = op_id,
                callout_index = index,
                kind = CalloutKind::of(callout).as_str(),
            ))
            .await;
        if let Some(live) = live {
            live.finish(&result);
        }
        result
    }

    async fn run_callout(
        &self,
        callout: &wit_types::Callout,
        op_id: u64,
    ) -> wit_types::CalloutResult {
        match callout {
            wit_types::Callout::Fetch(req) => {
                self.http
                    .fetch(req, crate::runtime::HTTP_FETCH_TIMEOUT)
                    .await
            },
            wit_types::Callout::FetchBlob(req) => self.blob.fetch(req).await,
            wit_types::Callout::GitOpenRepo(req) => self.git.open_repo(req, op_id),
            wit_types::Callout::OpenArchive(req) => self.archive.open(req).await,
            wit_types::Callout::ReadBlob(req) => self.blob.read(req),
        }
    }
}
