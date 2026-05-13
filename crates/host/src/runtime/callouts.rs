use crate::omnifs::provider::types as wit_types;
use crate::runtime::ProviderRuntime;
use crate::runtime::log_redaction::WitHeaders;
use tracing::Instrument;

#[derive(Debug, Clone, Copy)]
pub(crate) enum CalloutKind {
    HttpFetch,
    GitOpenRepo,
    BlobFetch,
    OpenArchive,
    ReadBlob,
    /// A WIT-defined callout the runtime knowingly does not implement
    /// yet (`stream-open`, `stream-recv`, `stream-close`, `ws-connect`,
    /// `ws-send`, `ws-recv`, `ws-close`). The provider gets a typed
    /// `callout-error{kind=internal, retryable=false}` back; the
    /// dispatch logs this as a known-unsupported variant, not as an
    /// unknown enum.
    Unsupported,
}

impl CalloutKind {
    pub(crate) fn of(callout: &wit_types::Callout) -> Self {
        match callout {
            wit_types::Callout::Fetch(_) => Self::HttpFetch,
            wit_types::Callout::GitOpenRepo(_) => Self::GitOpenRepo,
            wit_types::Callout::FetchBlob(_) => Self::BlobFetch,
            wit_types::Callout::OpenArchive(_) => Self::OpenArchive,
            wit_types::Callout::ReadBlob(_) => Self::ReadBlob,
            wit_types::Callout::StreamOpen(_)
            | wit_types::Callout::StreamRecv(_)
            | wit_types::Callout::StreamClose(_)
            | wit_types::Callout::WsConnect(_)
            | wit_types::Callout::WsSend(_)
            | wit_types::Callout::WsRecv(_)
            | wit_types::Callout::WsClose(_) => Self::Unsupported,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::HttpFetch => "http.fetch",
            Self::GitOpenRepo => "git.open_repo",
            Self::BlobFetch => "blob.fetch",
            Self::OpenArchive => "archive.open",
            Self::ReadBlob => "blob.read",
            Self::Unsupported => "unsupported",
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

pub(crate) fn unsupported_callout_variant(callout: &wit_types::Callout) -> &'static str {
    match callout {
        wit_types::Callout::StreamOpen(_) => "stream.open",
        wit_types::Callout::StreamRecv(_) => "stream.recv",
        wit_types::Callout::StreamClose(_) => "stream.close",
        wit_types::Callout::WsConnect(_) => "ws.connect",
        wit_types::Callout::WsSend(_) => "ws.send",
        wit_types::Callout::WsRecv(_) => "ws.recv",
        wit_types::Callout::WsClose(_) => "ws.close",
        _ => "unknown",
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
        _ => {},
    }
}

impl ProviderRuntime {
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
        self.run_callout(callout)
            .instrument(tracing::info_span!(
                target: "omnifs_callout",
                "callout",
                operation_id = op_id,
                callout_index = index,
                kind = CalloutKind::of(callout).as_str(),
            ))
            .await
    }

    async fn run_callout(&self, callout: &wit_types::Callout) -> wit_types::CalloutResult {
        match callout {
            wit_types::Callout::Fetch(req) => self.http.fetch(req).await,
            wit_types::Callout::FetchBlob(req) => self.blob.fetch(req).await,
            wit_types::Callout::GitOpenRepo(req) => self.git.open_repo(req),
            wit_types::Callout::OpenArchive(req) => self.archive.open(req).await,
            wit_types::Callout::ReadBlob(req) => self.blob.read(req),
            wit_types::Callout::StreamOpen(_)
            | wit_types::Callout::StreamRecv(_)
            | wit_types::Callout::StreamClose(_)
            | wit_types::Callout::WsConnect(_)
            | wit_types::Callout::WsSend(_)
            | wit_types::Callout::WsRecv(_)
            | wit_types::Callout::WsClose(_) => self.unsupported_callout(callout),
        }
    }

    #[allow(clippy::unused_self)]
    #[tracing::instrument(target = "omnifs_callout", skip_all, fields(
        unsupported_variant = unsupported_callout_variant(callout),
        error.kind = tracing::field::Empty,
        error.message = tracing::field::Empty,
        error.retryable = tracing::field::Empty,
    ))]
    fn unsupported_callout(&self, callout: &wit_types::Callout) -> wit_types::CalloutResult {
        let variant = unsupported_callout_variant(callout);
        tracing::warn!(
            target: "omnifs_callout",
            variant,
            "callout variant not implemented",
        );
        let result = callout_internal("callout type not yet implemented");
        record_outcome(&result);
        result
    }
}
