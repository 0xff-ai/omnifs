//! Structured inspector records produced by a tracing subscriber layer.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use omnifs_api::events::{
    CacheKind, CalloutKind, InspectorEvent, InspectorLineWriter, InspectorOutcome, InspectorRecord,
    OpEnd, OutcomeFields, TraceId,
};
use tokio::sync::broadcast;
use tracing::Event;
use tracing::Subscriber;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::warn;
use tracing::{Level, Span};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::{LookupSpan, SpanRef};

use omnifs_wit::provider::types as wit_types;

const DEFAULT_HISTORY_CAP: usize = 1024;
const DEFAULT_BROADCAST_CAP: usize = 256;
const TARGET: &str = "omnifs_inspector";

static GLOBAL: OnceLock<Option<Arc<Inspector>>> = OnceLock::new();

/// Install the process-wide inspector configuration once. Repeated calls
/// return the same instance, which keeps daemon startup idempotent.
pub fn init_global_from_env() -> Option<Arc<Inspector>> {
    GLOBAL
        .get_or_init(|| Inspector::open(InspectorConfig::from_env()).map(Arc::new))
        .clone()
}

#[derive(Debug, Clone)]
struct InspectorConfig {
    enabled: bool,
    history_cap: usize,
    broadcast_cap: usize,
    tee_path: Option<PathBuf>,
}

impl InspectorConfig {
    fn from_env() -> Self {
        Self {
            enabled: !disabled_from_env(),
            history_cap: parse_env_positive_usize("OMNIFS_INSPECTOR_HISTORY_CAP")
                .unwrap_or(DEFAULT_HISTORY_CAP),
            broadcast_cap: parse_env_positive_usize("OMNIFS_INSPECTOR_BROADCAST_CAP")
                .unwrap_or(DEFAULT_BROADCAST_CAP),
            tee_path: std::env::var("OMNIFS_INSPECTOR_PATH")
                .ok()
                .map(PathBuf::from),
        }
    }
}

type HistoryRing = Mutex<VecDeque<Arc<InspectorRecord>>>;
type FileTee = Mutex<InspectorLineWriter>;

/// In-memory inspector history and live stream. Records are created only by
/// [`InspectorLayer`], while this type owns their retention and delivery.
pub struct Inspector {
    history: HistoryRing,
    history_cap: usize,
    tee: Option<FileTee>,
    live: broadcast::Sender<Arc<InspectorRecord>>,
    process_start: Instant,
    next_trace: AtomicU64,
    next_seq: AtomicU64,
}

/// A tracing subscriber layer which translates the stable inspector spans and
/// events into the existing [`InspectorRecord`] wire shape.
pub struct InspectorLayer {
    inspector: Arc<Inspector>,
}

impl InspectorLayer {
    fn span_state(&self, name: &str, visitor: &Fields, inherited: Inherited) -> Option<SpanState> {
        let (kind, trace_id) = match name {
            "namespace.request" => (
                SpanKind::Namespace {
                    operation: visitor.string("operation"),
                    mount: visitor.string("mount"),
                    path: visitor.string("path"),
                },
                Some(self.inspector.next_trace_id()),
            ),
            "provider.operation" => {
                let mount = visitor.string("mount");
                let path = visitor.string("path");
                (
                    SpanKind::Provider {
                        operation_id: visitor.u64("operation_id"),
                        mount: if mount.is_empty() {
                            inherited.mount.clone()
                        } else {
                            mount
                        },
                        provider: visitor.string("provider"),
                        method: visitor.string("method"),
                        path: if path.is_empty() {
                            inherited.path.clone()
                        } else {
                            path
                        },
                    },
                    inherited.trace_id,
                )
            },
            "provider.callout" => (
                SpanKind::Callout {
                    operation_id: visitor.u64("operation_id"),
                    index: u32::try_from(visitor.u64("callout_index")).unwrap_or(u32::MAX),
                    kind: CalloutKind::from_field(&visitor.string("callout_kind"))?,
                    summary: visitor.string("summary"),
                },
                inherited.trace_id,
            ),
            "provider.subtree" => (
                SpanKind::Subtree {
                    operation_id: visitor.u64("operation_id"),
                    tree_ref: visitor.u64("tree_ref"),
                },
                inherited.trace_id,
            ),
            "provider.clone" => (
                SpanKind::Clone {
                    operation_id: visitor.u64("operation_id"),
                    cache_key: visitor.string("cache_key"),
                },
                inherited.trace_id,
            ),
            _ => return None,
        };
        Some(SpanState {
            kind: kind.clone(),
            trace_id,
            started: Instant::now(),
            outcome: visitor.outcome,
            mount: match &kind {
                SpanKind::Namespace { mount, .. } | SpanKind::Provider { mount, .. } => {
                    mount.clone()
                },
                _ => inherited.mount,
            },
            path: match &kind {
                SpanKind::Namespace { path, .. } | SpanKind::Provider { path, .. } => path.clone(),
                _ => inherited.path,
            },
            operation_id: match &kind {
                SpanKind::Provider { operation_id, .. }
                | SpanKind::Callout { operation_id, .. }
                | SpanKind::Subtree { operation_id, .. }
                | SpanKind::Clone { operation_id, .. } => Some(*operation_id),
                SpanKind::Namespace { .. } => inherited.operation_id,
            },
        })
    }
}

pub struct Subscription {
    pub history: Vec<Arc<InspectorRecord>>,
    pub live: broadcast::Receiver<Arc<InspectorRecord>>,
}

impl Inspector {
    fn open(config: InspectorConfig) -> Option<Self> {
        if !config.enabled {
            return None;
        }
        let tee = config
            .tee_path
            .and_then(|path| match InspectorLineWriter::open(&path) {
                Ok(writer) => Some(Mutex::new(writer)),
                Err(error) => {
                    warn!(%error, path = %path.display(), "failed to open inspector file tee");
                    None
                },
            });
        let (live, _) = broadcast::channel(config.broadcast_cap);
        Some(Self {
            history: Mutex::new(VecDeque::with_capacity(config.history_cap)),
            history_cap: config.history_cap,
            tee,
            live,
            process_start: Instant::now(),
            next_trace: AtomicU64::new(1),
            next_seq: AtomicU64::new(1),
        })
    }

    /// Build the layer which records spans emitted through this inspector.
    pub fn layer(self: &Arc<Self>) -> InspectorLayer {
        InspectorLayer {
            inspector: Arc::clone(self),
        }
    }

    pub fn subscribe(&self) -> Subscription {
        let history = self.history.lock().expect("inspector history lock");
        let live = self.live.subscribe();
        Subscription {
            history: history.iter().cloned().collect(),
            live,
        }
    }

    pub fn tee_path(&self) -> Option<PathBuf> {
        self.tee
            .as_ref()
            .map(|tee| tee.lock().expect("live tee lock").path().to_path_buf())
    }

    pub fn history_snapshot(&self) -> Vec<Arc<InspectorRecord>> {
        self.history
            .lock()
            .expect("inspector history lock")
            .iter()
            .cloned()
            .collect()
    }

    fn next_trace_id(&self) -> TraceId {
        self.next_trace.fetch_add(1, Ordering::Relaxed)
    }

    fn emit(&self, trace_id: TraceId, event: InspectorEvent) {
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let record = Arc::new(
            InspectorRecord::new(
                wall_ts(),
                to_us(self.process_start.elapsed()),
                trace_id,
                event,
            )
            .with_seq(seq),
        );
        {
            let mut history = self.history.lock().expect("inspector history lock");
            if history.len() == self.history_cap {
                history.pop_front();
            }
            history.push_back(Arc::clone(&record));
            // Keep history publication and live delivery atomic with respect
            // to `subscribe`, so a record is observed exactly once across the
            // snapshot/live boundary.
            let _ = self.live.send(Arc::clone(&record));
        }
        if let Some(tee) = &self.tee
            && let Ok(mut writer) = tee.lock()
            && let Err(error) = writer.write_record(&record)
        {
            warn!(%error, "failed to write inspector record to tee");
        }
    }

    fn emit_end(&self, state: &SpanState, trace_id: TraceId) {
        let result =
            OutcomeFields::with_outcome(state.outcome.unwrap_or(InspectorOutcome::Internal));
        let elapsed = to_us(state.started.elapsed());
        let end = OpEnd {
            elapsed_us: elapsed,
            result,
        };
        let event = match &state.kind {
            SpanKind::Namespace { operation, .. } => InspectorEvent::FuseEnd {
                op: operation.clone(),
                end,
            },
            SpanKind::Provider { operation_id, .. } => InspectorEvent::ProviderEnd {
                operation_id: *operation_id,
                end,
            },
            SpanKind::Callout {
                operation_id,
                index,
                ..
            } => InspectorEvent::CalloutEnd {
                operation_id: *operation_id,
                callout_index: *index,
                end,
            },
            SpanKind::Subtree {
                operation_id,
                tree_ref,
            } => InspectorEvent::SubtreeEnd {
                operation_id: *operation_id,
                tree_ref: format!("tree:{tree_ref}"),
                end,
            },
            SpanKind::Clone {
                operation_id,
                cache_key,
            } => InspectorEvent::CloneEnd {
                operation_id: *operation_id,
                cache_key: cache_key.clone(),
                end,
            },
        };
        self.emit(trace_id, event);
    }
}

impl<S> Layer<S> for InspectorLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let metadata = attrs.metadata();
        if metadata.target() != TARGET {
            return;
        }
        let mut visitor = Fields::default();
        attrs.record(&mut visitor);
        let parent = if attrs.is_root() {
            None
        } else {
            ctx.span(id).and_then(|span| span.parent())
        };
        let inherited = nearest_parent_state(parent);
        let Some(state) = self.span_state(metadata.name(), &visitor, inherited) else {
            return;
        };
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(state.clone());
        }
        let Some(trace_id) = state.trace_id else {
            return;
        };
        let event = match &state.kind {
            SpanKind::Namespace {
                operation,
                mount,
                path,
            } => InspectorEvent::FuseStart {
                op: operation.clone(),
                mount: mount.clone(),
                path: path.clone(),
            },
            SpanKind::Provider {
                operation_id,
                mount,
                provider,
                method,
                path,
            } => InspectorEvent::ProviderStart {
                operation_id: *operation_id,
                mount: mount.clone(),
                provider: provider.clone(),
                method: method.clone(),
                path: path.clone(),
            },
            SpanKind::Callout {
                operation_id,
                index,
                kind,
                summary,
            } => InspectorEvent::CalloutStart {
                operation_id: *operation_id,
                callout_index: *index,
                kind: *kind,
                summary: summary.clone(),
            },
            SpanKind::Subtree {
                operation_id,
                tree_ref,
            } => InspectorEvent::SubtreeStart {
                operation_id: *operation_id,
                tree_ref: format!("tree:{tree_ref}"),
            },
            SpanKind::Clone {
                operation_id,
                cache_key,
            } => InspectorEvent::CloneStart {
                operation_id: *operation_id,
                cache_key: cache_key.clone(),
                remote: visitor.string("remote"),
            },
        };
        self.inspector.emit(trace_id, event);
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        let mut visitor = Fields::default();
        values.record(&mut visitor);
        if let Some(span) = ctx.span(id)
            && let Some(state) = span.extensions_mut().get_mut::<SpanState>()
            && visitor.outcome.is_some()
        {
            state.outcome = visitor.outcome;
        }
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(&id) else { return };
        let Some(state) = span.extensions().get::<SpanState>().cloned() else {
            return;
        };
        let Some(trace_id) = state.trace_id else {
            return;
        };
        self.inspector.emit_end(&state, trace_id);
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        if event.metadata().target() != TARGET || event.metadata().name() != "cache.activity" {
            return;
        }
        let mut visitor = Fields::default();
        event.record(&mut visitor);
        let Some(kind) = visitor
            .string_opt("cache_kind")
            .and_then(|v| CacheKind::from_field(&v))
        else {
            return;
        };
        let Some(current) = ctx.lookup_current() else {
            return;
        };
        let inherited = current.extensions().get::<SpanState>().map_or_else(
            || nearest_parent_state(current.parent()),
            |state| Inherited {
                trace_id: state.trace_id,
                mount: state.mount.clone(),
                path: state.path.clone(),
                operation_id: state.operation_id,
            },
        );
        let Some(trace_id) = inherited.trace_id else {
            return;
        };
        self.inspector.emit(
            trace_id,
            InspectorEvent::CacheEvent {
                operation_id: inherited.operation_id,
                mount: inherited.mount,
                path: inherited.path,
                kind,
                elapsed_us: None,
            },
        );
    }
}

#[derive(Clone)]
struct SpanState {
    kind: SpanKind,
    trace_id: Option<TraceId>,
    started: Instant,
    outcome: Option<InspectorOutcome>,
    mount: String,
    path: String,
    operation_id: Option<u64>,
}

#[derive(Clone)]
enum SpanKind {
    Namespace {
        operation: String,
        mount: String,
        path: String,
    },
    Provider {
        operation_id: u64,
        mount: String,
        provider: String,
        method: String,
        path: String,
    },
    Callout {
        operation_id: u64,
        index: u32,
        kind: CalloutKind,
        summary: String,
    },
    Subtree {
        operation_id: u64,
        tree_ref: u64,
    },
    Clone {
        operation_id: u64,
        cache_key: String,
    },
}

#[derive(Default)]
struct Inherited {
    trace_id: Option<TraceId>,
    mount: String,
    path: String,
    operation_id: Option<u64>,
}

fn nearest_parent_state<'a, S>(mut parent: Option<SpanRef<'a, S>>) -> Inherited
where
    S: Subscriber + for<'b> LookupSpan<'b>,
{
    while let Some(span) = parent {
        if let Some(state) = span.extensions().get::<SpanState>() {
            return Inherited {
                trace_id: state.trace_id,
                mount: state.mount.clone(),
                path: state.path.clone(),
                operation_id: state.operation_id,
            };
        }
        parent = span.parent();
    }
    Inherited::default()
}

#[derive(Default)]
struct Fields {
    operation: Option<String>,
    mount: Option<String>,
    path: Option<String>,
    provider: Option<String>,
    method: Option<String>,
    callout_kind: Option<String>,
    summary: Option<String>,
    tree_ref: Option<String>,
    cache_key: Option<String>,
    remote: Option<String>,
    cache_kind: Option<String>,
    operation_id: Option<u64>,
    callout_index: Option<u64>,
    tree_ref_id: Option<u64>,
    outcome: Option<InspectorOutcome>,
}

impl Fields {
    fn string(&self, name: &str) -> String {
        self.string_opt(name).unwrap_or_default()
    }
    fn string_opt(&self, name: &str) -> Option<String> {
        match name {
            "operation" => self.operation.clone(),
            "mount" => self.mount.clone(),
            "path" => self.path.clone(),
            "provider" => self.provider.clone(),
            "method" => self.method.clone(),
            "callout_kind" => self.callout_kind.clone(),
            "summary" => self.summary.clone(),
            "tree_ref" => self.tree_ref.clone(),
            "cache_key" => self.cache_key.clone(),
            "remote" => self.remote.clone(),
            "cache_kind" => self.cache_kind.clone(),
            _ => None,
        }
    }
    fn u64(&self, name: &str) -> u64 {
        match name {
            "operation_id" => self.operation_id.unwrap_or_default(),
            "callout_index" => self.callout_index.unwrap_or_default(),
            "tree_ref" => self.tree_ref_id.unwrap_or_default(),
            _ => 0,
        }
    }
}

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let _ = (field, value);
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "outcome" {
            self.outcome = InspectorOutcome::from_field(value);
        }
        let slot = match field.name() {
            "operation" => &mut self.operation,
            "mount" => &mut self.mount,
            "path" => &mut self.path,
            "provider" => &mut self.provider,
            "method" => &mut self.method,
            "callout_kind" => &mut self.callout_kind,
            "summary" => &mut self.summary,
            "tree_ref" => &mut self.tree_ref,
            "cache_key" => &mut self.cache_key,
            "remote" => &mut self.remote,
            "cache_kind" => &mut self.cache_kind,
            _ => return,
        };
        *slot = Some(value.to_string());
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        if value >= 0 {
            self.record_u64(field, value.cast_unsigned());
        }
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        match field.name() {
            "operation_id" => self.operation_id = Some(value),
            "callout_index" => self.callout_index = Some(value),
            "tree_ref" => self.tree_ref_id = Some(value),
            _ => {},
        }
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        let _ = (field, value);
    }
}

/// Create the structured root span for one frontend filesystem request.
pub fn request_span(operation: &'static str, mount: &str, path: &str) -> Span {
    if !tracing::enabled!(target: TARGET, Level::INFO) {
        return Span::none();
    }
    tracing::info_span!(target: TARGET, "namespace.request", operation = operation, mount = mount, path = path, outcome = tracing::field::Empty)
}

pub(crate) fn provider_span(
    operation_id: u64,
    mount: &str,
    provider: &str,
    method: &str,
    path: &str,
) -> Span {
    if !tracing::enabled!(target: TARGET, Level::INFO) {
        return Span::none();
    }
    tracing::info_span!(target: TARGET, "provider.operation", operation_id, mount, provider, method, path, outcome = tracing::field::Empty)
}

pub(crate) fn callout_span(callout: &wit_types::Callout, operation_id: u64, index: usize) -> Span {
    if !tracing::enabled!(target: TARGET, Level::INFO) {
        return Span::none();
    }
    let view = WitCalloutView(callout);
    tracing::info_span!(target: TARGET, "provider.callout", operation_id, callout_index = u32::try_from(index).unwrap_or(u32::MAX), callout_kind = view.kind().as_str(), summary = view.summary(), outcome = tracing::field::Empty)
}

pub(crate) fn record_subtree_handoff(operation_id: u64, tree_ref: u64) {
    if !tracing::enabled!(target: TARGET, Level::INFO) {
        return;
    }
    let span = tracing::info_span!(target: TARGET, "provider.subtree", operation_id, tree_ref, outcome = tracing::field::Empty);
    let _entered = span.enter();
    record_outcome(&span, InspectorOutcome::Ok);
}

pub(crate) fn clone_span(operation_id: u64, cache_id: &str, clone_url: &str) -> Span {
    if !tracing::enabled!(target: TARGET, Level::INFO) {
        return Span::none();
    }
    let remote = omnifs_api::events::redact_git_remote(clone_url);
    tracing::info_span!(target: TARGET, "provider.clone", operation_id, cache_id, remote, outcome = tracing::field::Empty)
}

/// Record the terminal typed outcome which the layer emits when `span` closes.
pub fn record_outcome(span: &Span, outcome: InspectorOutcome) {
    span.record("outcome", outcome.as_str());
}

/// Emit cache activity against the nearest active Inspector request span.
pub fn cache_event(kind: CacheKind) {
    tracing::event!(name: "cache.activity", target: TARGET, Level::INFO, cache_kind = kind.as_str());
}

pub(crate) fn outcome_for_callout(result: &wit_types::CalloutResult) -> InspectorOutcome {
    match result {
        wit_types::CalloutResult::HttpResponse(response) => match response.status {
            200..=299 => InspectorOutcome::Ok,
            404 => InspectorOutcome::NotFound,
            401 | 403 => InspectorOutcome::Denied,
            429 => InspectorOutcome::Timeout,
            _ => InspectorOutcome::Network,
        },
        wit_types::CalloutResult::BlobFetched(_)
        | wit_types::CalloutResult::GitRepoOpened(_)
        | wit_types::CalloutResult::ArchiveOpened(_)
        | wit_types::CalloutResult::BlobRead(_) => InspectorOutcome::Ok,
        wit_types::CalloutResult::CalloutError(error) => error_kind_outcome(error.kind),
    }
}

pub(crate) fn outcome_for_provider_error(error: &wit_types::ProviderError) -> InspectorOutcome {
    error_kind_outcome(error.kind)
}

struct WitCalloutView<'a>(&'a wit_types::Callout);

impl WitCalloutView<'_> {
    fn kind(&self) -> CalloutKind {
        match self.0 {
            wit_types::Callout::Fetch(_) => CalloutKind::Fetch,
            wit_types::Callout::FetchBlob(_) => CalloutKind::FetchBlob,
            wit_types::Callout::GitOpenRepo(_) => CalloutKind::GitOpenRepo,
            wit_types::Callout::OpenArchive(_) => CalloutKind::OpenArchive,
            wit_types::Callout::ReadBlob(_) => CalloutKind::ReadBlob,
        }
    }
    fn summary(&self) -> String {
        match self.0 {
            wit_types::Callout::Fetch(req) => {
                omnifs_api::events::redact_http_url_for_summary(req.method.as_str(), &req.url)
            },
            wit_types::Callout::FetchBlob(req) => {
                omnifs_api::events::redact_http_url_for_summary(req.method.as_str(), &req.url)
            },
            wit_types::Callout::GitOpenRepo(req) => format!(
                "git.open_repo {}",
                omnifs_api::events::redact_git_remote(&req.clone_url)
            ),
            wit_types::Callout::OpenArchive(req) => format!(
                "archive.open blob={} strip={}",
                req.blob,
                req.strip_prefix.as_deref().unwrap_or("")
            ),
            wit_types::Callout::ReadBlob(req) => {
                format!("blob.read {}B @ {}", req.len.unwrap_or(0), req.offset)
            },
        }
    }
}

fn error_kind_outcome(kind: wit_types::ErrorKind) -> InspectorOutcome {
    match kind {
        wit_types::ErrorKind::NotFound => InspectorOutcome::NotFound,
        wit_types::ErrorKind::NotADirectory
        | wit_types::ErrorKind::NotAFile
        | wit_types::ErrorKind::InvalidInput => InspectorOutcome::InvalidInput,
        wit_types::ErrorKind::PermissionDenied | wit_types::ErrorKind::Denied => {
            InspectorOutcome::Denied
        },
        wit_types::ErrorKind::TooLarge => InspectorOutcome::TooLarge,
        wit_types::ErrorKind::RateLimited | wit_types::ErrorKind::Timeout => {
            InspectorOutcome::Timeout
        },
        wit_types::ErrorKind::Network => InspectorOutcome::Network,
        wit_types::ErrorKind::VersionMismatch | wit_types::ErrorKind::Internal => {
            InspectorOutcome::Internal
        },
    }
}

fn wall_ts() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

fn to_us(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}
fn disabled_from_env() -> bool {
    matches!(
        std::env::var("OMNIFS_INSPECTOR").ok().as_deref(),
        Some("0" | "false" | "off" | "no")
    )
}
fn parse_env_positive_usize(name: &str) -> Option<usize> {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|n: &usize| *n > 0)
}

#[cfg(test)]
impl Inspector {
    fn new_for_test(history_cap: usize) -> Self {
        Self::open(InspectorConfig {
            enabled: true,
            history_cap,
            broadcast_cap: DEFAULT_BROADCAST_CAP,
            tee_path: None,
        })
        .expect("enabled")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future;
    use std::future::Future as _;
    use std::sync::Barrier;
    use std::task::{Context as TaskContext, Poll};
    use std::thread;
    use tracing::Instrument as _;
    use tracing_subscriber::{Registry, layer::SubscriberExt};

    #[test]
    fn hierarchy_emits_once_and_inherits_trace() {
        let inspector = Arc::new(Inspector::new_for_test(16));
        let layer = inspector.layer();
        tracing::subscriber::with_default(Registry::default().with(layer), || {
            let request = request_span("lookup", "m", "/x");
            let _enter = request.enter();
            {
                let bridge = tracing::info_span!("runtime.driver");
                let _bridge_enter = bridge.enter();
                let provider = provider_span(7, "", "p", "read_chunk", "");
                let _provider_enter = provider.enter();
                record_outcome(&provider, InspectorOutcome::Ok);
            }
            record_outcome(&request, InspectorOutcome::Ok);
        });
        let events = inspector.history_snapshot();
        assert_eq!(events.len(), 4);
        assert!(events.iter().all(|record| record.trace_id == 1));
        assert!(matches!(events[0].event, InspectorEvent::FuseStart { .. }));
        assert!(matches!(
            &events[1].event,
            InspectorEvent::ProviderStart { mount, path, .. }
                if mount == "m" && path == "/x"
        ));
    }

    #[test]
    fn dropped_instrumented_future_closes_internal_once() {
        let inspector = Arc::new(Inspector::new_for_test(16));
        tracing::subscriber::with_default(Registry::default().with(inspector.layer()), || {
            let mut pending =
                Box::pin(future::pending::<()>().instrument(request_span("lookup", "m", "/x")));
            let waker = futures::task::noop_waker();
            let mut cx = TaskContext::from_waker(&waker);
            assert_eq!(pending.as_mut().poll(&mut cx), Poll::Pending);
            drop(pending);
        });
        let events = inspector.history_snapshot();
        assert_eq!(events.len(), 2);
        assert!(
            matches!(&events[1].event, InspectorEvent::FuseEnd { end, .. } if end.result.outcome == InspectorOutcome::Internal)
        );
    }

    #[test]
    fn subscriber_sees_history_snapshot_and_future_events() {
        let inspector = Arc::new(Inspector::new_for_test(8));
        let subscriber = Registry::default().with(inspector.layer());
        tracing::subscriber::with_default(subscriber, || {
            for path in ["/a", "/b"] {
                let span = request_span("lookup", "m", path);
                record_outcome(&span, InspectorOutcome::Ok);
            }
        });

        let mut subscription = inspector.subscribe();
        assert_eq!(
            subscription
                .history
                .iter()
                .map(|record| record.seq)
                .collect::<Vec<_>>(),
            vec![1, 2, 3, 4]
        );

        tracing::subscriber::with_default(Registry::default().with(inspector.layer()), || {
            let span = request_span("lookup", "m", "/c");
            record_outcome(&span, InspectorOutcome::Ok);
        });
        assert_eq!(subscription.live.try_recv().expect("live start").seq, 5);
        assert_eq!(subscription.live.try_recv().expect("live end").seq, 6);
    }

    #[test]
    fn concurrent_retention_remains_bounded() {
        let inspector = Arc::new(Inspector::new_for_test(64));
        let threads = 8;
        let per_thread = 100;
        let barrier = Arc::new(Barrier::new(threads + 1));
        let mut handles = Vec::new();
        for thread_id in 0..threads {
            let inspector = Arc::clone(&inspector);
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                barrier.wait();
                for index in 0..per_thread {
                    inspector.emit(
                        u64::try_from(thread_id * per_thread + index).expect("trace id"),
                        InspectorEvent::FuseStart {
                            op: "lookup".into(),
                            mount: "test".into(),
                            path: "/x".into(),
                        },
                    );
                }
            }));
        }
        barrier.wait();
        for _ in 0..100 {
            assert!(inspector.history_snapshot().len() <= 64);
        }
        for handle in handles {
            handle.join().expect("emitter thread");
        }
        assert_eq!(inspector.history_snapshot().len(), 64);
        assert_eq!(
            inspector.next_seq.load(Ordering::Relaxed) - 1,
            u64::try_from(threads * per_thread).expect("emitted count")
        );
    }

    #[test]
    fn fetch_callout_summary_strips_query_and_authorization() {
        let callout = wit_types::Callout::Fetch(wit_types::HttpRequest {
            method: "GET".to_string(),
            url: "https://api.github.com/repos/o/r?access_token=secret-token&page=2".to_string(),
            headers: vec![wit_types::Header {
                name: "Authorization".to_string(),
                value: "Bearer super-secret".to_string(),
            }],
            body: None,
        });

        let summary = WitCalloutView(&callout).summary();

        assert_eq!(summary, "GET api.github.com/repos/o/r");
        assert!(!summary.contains('?'));
        assert!(!summary.contains("secret-token"));
        assert!(!summary.contains("super-secret"));
    }
}
