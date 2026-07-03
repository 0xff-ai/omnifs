//! Per-callout dispatch and the tracing surface for provider callouts.
//!
//! # Dispatch
//!
//! `CalloutHost` implements the host side of the provider's async callout
//! imports. Each import routes one callout variant to its executor and returns
//! the matching `CalloutResult` directly to the suspended component future.
//! `CalloutHost::run` is exhaustive; adding a WIT callout requires adding a
//! real executor arm.
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
//! Each callout produces an outer `callout` span from `CalloutHost::dispatch`
//! with operation id, callout index, and kind, plus an executor span on the
//! public method that owns request and outcome fields for that callout. URL and
//! header fields render through redacting display wrappers.
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
//! 2. Add a new `CalloutKind` variant + strum label in this file.
//! 3. Extend `CalloutKind::of` exhaustively (no wildcard arm).
//! 4. Add the executor public method with
//!    `#[tracing::instrument(target = "omnifs_callout", skip_all,
//!    fields(...))]` listing all fields (use `field::Empty` for
//!    late-bound ones) and calling `record_outcome(&result)` before
//!    return.
//! 5. Add a `CalloutHost::run` arm dispatching to it.

pub(crate) mod archive;
pub(crate) mod blob;
pub mod cloner;
pub(crate) mod git;
pub(crate) mod http;
pub(crate) mod wit_convert;

use crate::archive::ArchiveExecutor;
use crate::blob::BlobExecutor;
use crate::git::GitExecutor;
use crate::http::HttpStack;
use crate::inspector::InspectorCallout;
use crate::log_redaction::WitHeaders;
use omnifs_wit::provider::types as wit_types;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use tracing::Instrument;

#[derive(Debug, Clone, Copy, strum::IntoStaticStr)]
pub(crate) enum CalloutKind {
    #[strum(serialize = "http.fetch")]
    HttpFetch,
    #[strum(serialize = "git.open_repo")]
    GitOpenRepo,
    #[strum(serialize = "blob.fetch")]
    BlobFetch,
    #[strum(serialize = "archive.open")]
    OpenArchive,
    #[strum(serialize = "blob.read")]
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

/// One item on the [`TestCallouts`] capture channel: a captured provider
/// callout awaiting an answer, or a marker that the provider instance's
/// executor has gone idle. The `Parked` marker demarcates a callout burst
/// deterministically. The instance runs on a single-threaded executor, so
/// every callout a round issues is enqueued before that executor can park;
/// the marker therefore follows the round's last callout in FIFO order, and
/// the harness reads the burst boundary instead of guessing it from the gap
/// between two enqueues.
pub(crate) enum TestSignal {
    Callout(TestCallout),
    Parked,
}

/// Test-only capture channel for provider integration tests that inspect
/// outbound HTTP/blob imports before answering them.
#[derive(Clone)]
pub(crate) struct TestCallouts {
    tx: mpsc::Sender<TestSignal>,
}

pub(crate) struct TestCallout {
    pub(crate) op_id: u64,
    pub(crate) callout: wit_types::Callout,
    pub(crate) reply: tokio::sync::oneshot::Sender<wit_types::CalloutResult>,
}

/// Emits a [`TestSignal::Parked`] each time the provider instance's executor
/// goes idle. Installed as the tokio `on_thread_park` hook, so it fires only
/// at true quiescence: a self-woken task keeps the run queue non-empty and
/// does not park, so a park means the round's callouts are all enqueued and
/// the guest can make no further progress until one is answered.
#[derive(Clone)]
pub(crate) struct ParkSignal {
    tx: mpsc::Sender<TestSignal>,
}

impl ParkSignal {
    pub(crate) fn notify(&self) {
        let _ = self.tx.send(TestSignal::Parked);
    }
}

impl TestCallouts {
    pub(crate) fn channel() -> (Self, mpsc::Receiver<TestSignal>) {
        let (tx, rx) = mpsc::channel();
        (Self { tx }, rx)
    }

    pub(crate) fn park_signal(&self) -> ParkSignal {
        ParkSignal {
            tx: self.tx.clone(),
        }
    }

    async fn run(&self, op_id: u64, callout: wit_types::Callout) -> wit_types::CalloutResult {
        let (reply, response) = tokio::sync::oneshot::channel();
        if self
            .tx
            .send(TestSignal::Callout(TestCallout {
                op_id,
                callout,
                reply,
            }))
            .is_err()
        {
            return callout_internal("test callout receiver dropped");
        }
        match response.await {
            Ok(result) => result,
            Err(_) => callout_internal("test callout response dropped"),
        }
    }
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

#[derive(Clone)]
pub(crate) struct CalloutHost {
    http: Arc<HttpStack>,
    git: GitExecutor,
    blob: BlobExecutor,
    archive: Arc<ArchiveExecutor>,
    next_callout_index: Arc<AtomicUsize>,
    test_callouts: Option<TestCallouts>,
}

impl CalloutHost {
    pub(crate) fn new(
        http: Arc<HttpStack>,
        git: GitExecutor,
        blob: BlobExecutor,
        archive: Arc<ArchiveExecutor>,
    ) -> Self {
        Self {
            http,
            git,
            blob,
            archive,
            next_callout_index: Arc::new(AtomicUsize::new(0)),
            test_callouts: None,
        }
    }

    pub(crate) fn with_test_callouts(mut self, test_callouts: TestCallouts) -> Self {
        self.test_callouts = Some(test_callouts);
        self
    }

    pub(crate) async fn dispatch(
        &self,
        op_id: u64,
        callout: wit_types::Callout,
    ) -> wit_types::CalloutResult {
        let index = self.next_callout_index.fetch_add(1, Ordering::Relaxed);
        let live = InspectorCallout::begin(&callout, op_id, index);
        let kind: &'static str = CalloutKind::of(&callout).into();
        let result = self
            .run(&callout, op_id)
            .instrument(tracing::info_span!(
                target: "omnifs_callout",
                "callout",
                operation_id = op_id,
                callout_index = index,
                kind = kind,
            ))
            .await;
        if let Some(live) = live {
            live.finish(&result);
        }
        result
    }

    async fn run(&self, callout: &wit_types::Callout, op_id: u64) -> wit_types::CalloutResult {
        if let Some(test_callouts) = &self.test_callouts
            && matches!(
                callout,
                wit_types::Callout::Fetch(_) | wit_types::Callout::FetchBlob(_)
            )
        {
            return test_callouts.run(op_id, callout.clone()).await;
        }
        match callout {
            wit_types::Callout::Fetch(req) => {
                self.http
                    .fetch(req, crate::runtime::HTTP_FETCH_TIMEOUT)
                    .await
            },
            wit_types::Callout::FetchBlob(req) => self.blob.fetch(req).await,
            // `open_repo` shells out to `git` and blocks (clone, fetch). Run it
            // off the single concurrent-store event-loop thread so other
            // in-flight provider ops on this instance keep progressing while it
            // runs; awaiting the blocking task yields `Pending` to the loop.
            wit_types::Callout::GitOpenRepo(req) => {
                let git = self.git.clone();
                let req = req.clone();
                let span = tracing::Span::current();
                spawn_blocking_callout("git.open_repo", move || {
                    span.in_scope(|| git.open_repo(&req, op_id))
                })
                .await
            },
            wit_types::Callout::OpenArchive(req) => self.archive.open(req).await,
            // Synchronous bounded disk read; offloaded for the same reason as
            // `git.open_repo` so a slow read never stalls the event loop.
            wit_types::Callout::ReadBlob(req) => {
                let blob = self.blob.clone();
                let req = *req;
                let span = tracing::Span::current();
                spawn_blocking_callout("blob.read", move || span.in_scope(|| blob.read(&req))).await
            },
        }
    }
}

/// Run a blocking executor call on the Tokio blocking pool and surface a join
/// failure as an internal `CalloutResult`. Keeps synchronous host work off the
/// component event-loop thread so concurrent provider ops are not serialized.
async fn spawn_blocking_callout(
    label: &'static str,
    f: impl FnOnce() -> wit_types::CalloutResult + Send + 'static,
) -> wit_types::CalloutResult {
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result,
        Err(join_error) => callout_internal(format!("{label} task failed: {join_error}")),
    }
}
