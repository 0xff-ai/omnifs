//! JSONL inspector stream for the `omnifs inspect` TUI.
//!
//! The host emits structured records at three call-graph boundaries: FUSE
//! requests ([`InspectorFuseScope`]), provider operations ([`InspectorProviderOp`]),
//! and individual callouts ([`InspectorCallout`]). Each is an RAII span: the
//! `start` event is emitted at construction, the `end` event on drop, and
//! the trace id flows through a thread-local so nested work correlates
//! without explicit threading.

use std::cell::Cell;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crossbeam_queue::ArrayQueue;
use omnifs_inspector::{
    CacheKind, CalloutKind, InspectorEvent, InspectorLineWriter, InspectorOutcome, InspectorRecord,
    OpEnd, OutcomeFields, TraceId, serialize_record,
};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::runtime::Handle;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::op::Op;
use omnifs_wit::provider::types as wit_types;

const DEFAULT_HISTORY_CAP: usize = 1024;
const DEFAULT_BROADCAST_CAP: usize = 256;
/// Default TCP loopback address the daemon binds for the inspector stream
/// subscribers. Docker-Desktop hosts reach this through a port
/// forward; native-Linux hosts reach it via the container's network
/// namespace bridge.
const DEFAULT_LIVE_ADDR: &str = "0.0.0.0:7878";

static GLOBAL: OnceLock<Arc<InspectorSink>> = OnceLock::new();

thread_local! {
    static CURRENT_TRACE: Cell<Option<TraceId>> = const { Cell::new(None) };
}

/// Install the process-wide live sink. Returns `None` when disabled.
pub fn init_global_from_env() -> Option<Arc<InspectorSink>> {
    let sink = InspectorSink::open(InspectorConfig::from_env())?;
    let arc = Arc::new(sink);
    let _ = GLOBAL.set(Arc::clone(&arc));
    Some(arc)
}

pub fn global() -> Option<Arc<InspectorSink>> {
    GLOBAL.get().cloned()
}

pub fn current_trace_id() -> Option<TraceId> {
    CURRENT_TRACE.get()
}

/// All daemon-side configuration knobs for the inspector stream,
/// resolved up-front so the sink and its socket server don't have to
/// re-read the environment at startup.
#[derive(Debug, Clone)]
pub struct InspectorConfig {
    /// Set to `false` by `OMNIFS_INSPECTOR=0|false|off|no` to disable the
    /// stream entirely. When false, all other fields are ignored.
    pub enabled: bool,
    /// History ring capacity. `OMNIFS_INSPECTOR_HISTORY_CAP` overrides.
    pub history_cap: usize,
    /// Broadcast channel capacity (per-subscriber lag tolerance).
    /// `OMNIFS_INSPECTOR_BROADCAST_CAP` overrides.
    pub broadcast_cap: usize,
    /// Optional opt-in file tee path. `OMNIFS_INSPECTOR_PATH` sets it.
    pub tee_path: Option<PathBuf>,
    /// TCP listen address for the subscriber server.
    /// `OMNIFS_INSPECTOR_ADDR` overrides; empty string disables.
    pub socket_addr: Option<SocketAddr>,
}

impl InspectorConfig {
    pub fn from_env() -> Self {
        Self {
            enabled: !disabled_from_env(),
            history_cap: parse_env_positive_usize("OMNIFS_INSPECTOR_HISTORY_CAP")
                .unwrap_or(DEFAULT_HISTORY_CAP),
            broadcast_cap: parse_env_positive_usize("OMNIFS_INSPECTOR_BROADCAST_CAP")
                .unwrap_or(DEFAULT_BROADCAST_CAP),
            tee_path: std::env::var("OMNIFS_INSPECTOR_PATH")
                .ok()
                .map(PathBuf::from),
            socket_addr: live_addr_from_env(),
        }
    }
}

/// In-memory ring buffer of recently emitted records. New subscribers
/// drain this on connect to see the recent past before they joined.
/// Wait-free MPMC; `push` returns `Err(record)` when full, at which
/// point we `pop` the oldest and retry.
type HistoryRing = ArrayQueue<Arc<InspectorRecord>>;

/// Optional file sink for opt-in recording. Mutex-protected; this is
/// the only blocking I/O path in [`InspectorSink::emit`], and it is off by
/// default. Production runs without it; dev runs may enable it via
/// `OMNIFS_INSPECTOR_PATH=<path>`.
type FileTee = Mutex<InspectorLineWriter>;

pub struct InspectorSink {
    history: HistoryRing,
    tee: Option<FileTee>,
    live: broadcast::Sender<Arc<InspectorRecord>>,
    process_start: Instant,
    next_trace: AtomicU64,
    next_seq: AtomicU64,
    dropped_history: AtomicU64,
    /// Address the subscriber server should bind on if `spawn_socket_server`
    /// is called. Resolved once at construction from `InspectorConfig`.
    socket_addr: Option<SocketAddr>,
}

/// One subscriber's view onto the sink: a snapshot of the history ring
/// at subscribe time, paired with a inspector broadcast receiver for events
/// emitted after the snapshot. Subscribers should sort or de-dup by
/// [`InspectorRecord::seq`] when both halves can contain the same record.
pub struct Subscription {
    pub history: Vec<Arc<InspectorRecord>>,
    pub live: broadcast::Receiver<Arc<InspectorRecord>>,
}

impl InspectorSink {
    /// Construct a sink from an explicit config. Returns `None` when
    /// the config is disabled. The file tee, if requested, is opened
    /// here so caller can log success/failure once at startup.
    pub fn open(config: InspectorConfig) -> Option<Self> {
        let InspectorConfig {
            enabled,
            history_cap,
            broadcast_cap,
            tee_path,
            socket_addr,
        } = config;
        if !enabled {
            return None;
        }
        let tee = tee_path.and_then(|path| match InspectorLineWriter::open(&path) {
            Ok(writer) => Some(Mutex::new(writer)),
            Err(error) => {
                warn!(%error, path = %path.display(), "failed to open inspector file tee");
                None
            },
        });
        let (live, _) = broadcast::channel(broadcast_cap);
        Some(Self {
            history: ArrayQueue::new(history_cap),
            tee,
            live,
            process_start: Instant::now(),
            next_trace: AtomicU64::new(1),
            next_seq: AtomicU64::new(1),
            dropped_history: AtomicU64::new(0),
            socket_addr,
        })
    }

    /// Attach a new subscriber. Captures the current history ring and
    /// returns a live receiver that yields every record emitted after
    /// the subscription point. The receiver may yield records that
    /// overlap the snapshot during the brief window between the two
    /// captures; the caller should de-dup by [`InspectorRecord::seq`].
    pub fn subscribe(&self) -> Subscription {
        let live = self.live.subscribe();
        let history = self.history_snapshot();
        Subscription { history, live }
    }

    /// File-tee path when one is configured. Returns `None` when
    /// recording is off (the default).
    pub fn tee_path(&self) -> Option<PathBuf> {
        self.tee
            .as_ref()
            .map(|t| t.lock().expect("live tee lock").path().to_path_buf())
    }

    /// Count of records dropped from the history ring due to capacity
    /// pressure. Surfaced for subscriber UIs.
    pub fn dropped_history(&self) -> u64 {
        self.dropped_history.load(Ordering::Relaxed)
    }

    /// Snapshot the history ring without disturbing future emission.
    /// Records are returned in approximate emission order; subscribers
    /// that need strict ordering should sort by [`InspectorRecord::seq`].
    pub fn history_snapshot(&self) -> Vec<Arc<InspectorRecord>> {
        let mut out = Vec::with_capacity(self.history.len());
        while let Some(record) = self.history.pop() {
            out.push(record);
        }
        // Re-insert into the ring so a subsequent subscriber still sees
        // the same history. Push order matches pop order, so the ring
        // is restored to its pre-snapshot state (no `dropped_history`
        // increment because we never exceeded capacity in steady state).
        // If a concurrent emit races the re-insert, drop the oldest of
        // the two to bias toward keeping the newest record.
        for record in &out {
            if let Err(extra) = self.history.push(Arc::clone(record))
                && let Some(oldest) = self.history.pop()
            {
                let _ = self.history.push(extra);
                drop(oldest);
            }
        }
        out
    }

    pub fn next_trace_id(&self) -> TraceId {
        self.next_trace.fetch_add(1, Ordering::Relaxed)
    }

    fn emit(&self, trace_id: TraceId, event: InspectorEvent) {
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let record = Arc::new(
            InspectorRecord::new(wall_ts(), self.mono_us(), trace_id, event).with_seq(seq),
        );

        // History ring: wait-free push, drop oldest on full.
        if let Err(rejected) = self.history.push(Arc::clone(&record)) {
            // Pop oldest to free a slot and retry. If a concurrent
            // subscriber-snapshot is draining, the pop may fail; in
            // that case we just drop the new record and count it.
            if self.history.pop().is_some() {
                self.dropped_history.fetch_add(1, Ordering::Relaxed);
                let _ = self.history.push(rejected);
            } else {
                self.dropped_history.fetch_add(1, Ordering::Relaxed);
            }
        }

        // Broadcast to live subscribers. send() is non-blocking; slow
        // subscribers fall behind and get Lagged on their next recv,
        // which they recover from independently. Returns Err only when
        // no receivers are attached — not a problem.
        let _ = self.live.send(Arc::clone(&record));

        // Opt-in file tee. Takes a Mutex but only when configured, so
        // production runs stay non-blocking.
        if let Some(tee) = self.tee.as_ref()
            && let Ok(mut writer) = tee.lock()
            && let Err(error) = writer.write_record(&record)
        {
            warn!(%error, "failed to write inspector record to tee");
        }
    }

    /// Bind a TCP loopback socket and spawn an accept loop that fans
    /// the live stream out to connected subscribers. Returns `None`
    /// when this sink was constructed without a socket address, or
    /// when the bind itself fails. The returned `JoinHandle` aborts
    /// the accept loop when dropped.
    pub fn spawn_socket_server(self: &Arc<Self>, rt: &Handle) -> Option<JoinHandle<()>> {
        let addr = self.socket_addr?;
        // Bind synchronously through std then convert; the FUSE mount runner
        // is on the tokio main thread, so `rt.block_on` here would nest
        // runtimes and panic.
        let std_listener = match std::net::TcpListener::bind(addr) {
            Ok(l) => l,
            Err(error) => {
                warn!(%error, %addr, "failed to bind inspector TCP socket");
                return None;
            },
        };
        if let Err(error) = std_listener.set_nonblocking(true) {
            warn!(%error, "failed to set listener nonblocking");
            return None;
        }
        let listener = match TcpListener::from_std(std_listener) {
            Ok(l) => l,
            Err(error) => {
                warn!(%error, "failed to register listener with tokio runtime");
                return None;
            },
        };
        Some(self.spawn_with_listener(listener, rt))
    }

    /// Like [`spawn_socket_server`] but takes an already-bound listener,
    /// useful for tests that need a free ephemeral port.
    pub fn spawn_with_listener(
        self: &Arc<Self>,
        listener: TcpListener,
        rt: &Handle,
    ) -> JoinHandle<()> {
        if let Ok(addr) = listener.local_addr() {
            debug!(%addr, "inspector TCP socket bound");
        }
        let sink = Arc::clone(self);
        rt.spawn(sink.accept_loop(listener))
    }

    /// Accept incoming connections and spawn one subscriber task per
    /// connection. Exits when the listener errors out or the task is
    /// aborted (e.g. on daemon shutdown).
    async fn accept_loop(self: Arc<Self>, listener: TcpListener) {
        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    tokio::spawn(self.subscribe().serve(stream));
                },
                Err(error) => {
                    warn!(%error, "inspector socket accept failed");
                    // Brief backoff; otherwise a persistent error spins.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                },
            }
        }
    }

    fn mono_us(&self) -> u64 {
        to_us(self.process_start.elapsed())
    }

    /// Emit a paired `subtree.start` / `subtree.end` for a handoff
    /// result. Tree-ref resolution is essentially a registry lookup;
    /// modelled as an instantaneous span with both events fired
    /// back-to-back so consumers see a discrete "this trace took a
    /// subtree handoff" record correlated by `trace_id` and
    /// `operation_id`.
    pub fn emit_subtree_handoff(
        &self,
        trace_id: TraceId,
        operation_id: u64,
        tree_ref: u64,
        elapsed: Duration,
    ) {
        let display = format!("tree:{tree_ref}");
        self.emit(
            trace_id,
            InspectorEvent::SubtreeStart {
                operation_id,
                tree_ref: display.clone(),
            },
        );
        self.emit(
            trace_id,
            InspectorEvent::SubtreeEnd {
                operation_id,
                tree_ref: display,
                end: OpEnd {
                    elapsed_us: to_us(elapsed),
                    result: OutcomeFields::ok(),
                },
            },
        );
    }
}

/// FUSE-bound trace scope; binds the thread-local trace id for nested
/// provider work and emits the `fuse.start`/`fuse.end` pair.
pub struct InspectorFuseScope {
    sink: Arc<InspectorSink>,
    trace_id: TraceId,
    op: &'static str,
    mount: String,
    path: String,
    start: Instant,
    outcome: Cell<Option<OutcomeFields>>,
}

impl InspectorFuseScope {
    pub fn begin(
        sink: Arc<InspectorSink>,
        op: &'static str,
        mount: impl Into<String>,
        path: impl Into<String>,
    ) -> Self {
        let mount = mount.into();
        let path = path.into();
        let trace_id = sink.next_trace_id();
        sink.emit(
            trace_id,
            InspectorEvent::FuseStart {
                op: op.to_string(),
                mount: mount.clone(),
                path: path.clone(),
            },
        );
        CURRENT_TRACE.set(Some(trace_id));
        Self {
            sink,
            trace_id,
            op,
            mount,
            path,
            start: Instant::now(),
            outcome: Cell::new(Some(OutcomeFields::ok())),
        }
    }

    pub fn trace_id(&self) -> TraceId {
        self.trace_id
    }

    pub fn emit_cache(&self, kind: CacheKind, elapsed: Duration) {
        self.sink.emit(
            self.trace_id,
            InspectorEvent::CacheEvent {
                operation_id: None,
                mount: self.mount.clone(),
                path: self.path.clone(),
                kind,
                elapsed_us: Some(to_us(elapsed)),
            },
        );
    }

    pub fn set_outcome(&self, outcome: InspectorOutcome) {
        self.outcome.set(Some(OutcomeFields::with_outcome(outcome)));
    }
}

impl Drop for InspectorFuseScope {
    fn drop(&mut self) {
        CURRENT_TRACE.set(None);
        let result = self
            .outcome
            .take()
            .unwrap_or_else(|| OutcomeFields::with_outcome(InspectorOutcome::Internal));
        self.sink.emit(
            self.trace_id,
            InspectorEvent::FuseEnd {
                op: self.op.to_string(),
                end: OpEnd {
                    elapsed_us: to_us(self.start.elapsed()),
                    result,
                },
            },
        );
    }
}

/// One in-flight provider operation. `provider.end` is emitted when the
/// span is consumed via [`Self::finish`], or as `Internal` on `Drop` if
/// the caller failed to finish it.
pub struct InspectorProviderOp {
    sink: Arc<InspectorSink>,
    trace_id: TraceId,
    operation_id: u64,
    start: Instant,
    outcome: Cell<Option<OutcomeFields>>,
}

impl InspectorProviderOp {
    /// Returns `None` when this op is not observable (e.g. `Initialize`),
    /// when there is no enclosing trace, or when the live sink is
    /// disabled. Callers can wrap the call in `Option::map`.
    pub fn begin(
        op: &Op,
        operation_id: u64,
        mount: &str,
        provider: &str,
        trace_id: TraceId,
    ) -> Option<Self> {
        let sink = global()?;
        let descriptor = op.live_descriptor()?;
        sink.emit(
            trace_id,
            InspectorEvent::ProviderStart {
                operation_id,
                mount: mount.to_string(),
                provider: provider.to_string(),
                method: descriptor.method.to_string(),
                path: descriptor.path,
            },
        );
        Some(Self {
            sink,
            trace_id,
            operation_id,
            start: Instant::now(),
            outcome: Cell::new(None),
        })
    }

    pub fn suspend(&self, callout_count: usize) {
        self.sink.emit(
            self.trace_id,
            InspectorEvent::ProviderSuspend {
                operation_id: self.operation_id,
                callout_count: clamp_u32(callout_count),
            },
        );
    }

    pub fn resume(&self, round: u32, result_count: usize) {
        self.sink.emit(
            self.trace_id,
            InspectorEvent::ProviderResume {
                operation_id: self.operation_id,
                round,
                result_count: clamp_u32(result_count),
            },
        );
    }

    /// Mark the outcome and emit `provider.end` on drop.
    pub fn finish(self, outcome: OutcomeFields) {
        self.outcome.set(Some(outcome));
    }
}

impl Drop for InspectorProviderOp {
    fn drop(&mut self) {
        let result = self
            .outcome
            .take()
            .unwrap_or_else(|| OutcomeFields::with_outcome(InspectorOutcome::Internal));
        self.sink.emit(
            self.trace_id,
            InspectorEvent::ProviderEnd {
                operation_id: self.operation_id,
                end: OpEnd {
                    elapsed_us: to_us(self.start.elapsed()),
                    result,
                },
            },
        );
    }
}

/// One in-flight callout. `callout.end` is emitted on drop using the
/// outcome supplied to [`Self::finish`].
pub struct InspectorCallout {
    sink: Arc<InspectorSink>,
    trace_id: TraceId,
    operation_id: u64,
    index: u32,
    start: Instant,
    outcome: Cell<Option<OutcomeFields>>,
}

impl InspectorCallout {
    /// Returns `None` when there is no enclosing trace or the sink is
    /// disabled.
    pub fn begin(callout: &wit_types::Callout, operation_id: u64, index: usize) -> Option<Self> {
        let trace_id = current_trace_id()?;
        let sink = global()?;
        let view = WitCalloutView(callout);
        let index = clamp_u32(index);
        sink.emit(
            trace_id,
            InspectorEvent::CalloutStart {
                operation_id,
                callout_index: index,
                kind: view.kind(),
                summary: view.summary(),
            },
        );
        Some(Self {
            sink,
            trace_id,
            operation_id,
            index,
            start: Instant::now(),
            outcome: Cell::new(None),
        })
    }

    /// Mark the outcome from the callout result and emit `callout.end` on drop.
    pub fn finish(self, result: &wit_types::CalloutResult) {
        self.outcome.set(Some(OutcomeFields::with_outcome(
            WitCalloutResultView(result).outcome(),
        )));
    }
}

impl Drop for InspectorCallout {
    fn drop(&mut self) {
        let result = self
            .outcome
            .take()
            .unwrap_or_else(|| OutcomeFields::with_outcome(InspectorOutcome::Internal));
        self.sink.emit(
            self.trace_id,
            InspectorEvent::CalloutEnd {
                operation_id: self.operation_id,
                callout_index: self.index,
                end: OpEnd {
                    elapsed_us: to_us(self.start.elapsed()),
                    result,
                },
            },
        );
    }
}

/// Inspect an `OpResult` to see whether it carries a subtree handoff.
/// Free function (not an `impl OpResult` method) because `OpResult` is
/// generated from the WIT bindings in another crate and orphan rules
/// block the impl.
pub fn subtree_tree_ref(result: &wit_types::OpResult) -> Option<u64> {
    match result {
        wit_types::OpResult::LookupChild(wit_types::LookupChildResult::Subtree(t))
        | wit_types::OpResult::ListChildren(wit_types::ListChildrenResult::Subtree(t)) => Some(*t),
        _ => None,
    }
}

/// `CloneObserver` impl that emits `clone.start`/`clone.end` records
/// against the currently-installed live sink. Notifications outside any
/// FUSE/provider span (no thread-local trace id) drop silently.
pub struct InspectorCloneObserver {
    operation_id: u64,
    trace_id: Option<TraceId>,
    sink: Option<Arc<InspectorSink>>,
}

impl InspectorCloneObserver {
    pub fn for_operation(operation_id: u64) -> Self {
        Self {
            operation_id,
            trace_id: current_trace_id(),
            sink: global(),
        }
    }
}

impl crate::cloner::CloneObserver for InspectorCloneObserver {
    fn on_clone_start(&mut self, cache_key: &str, clone_url: &str) {
        let (Some(trace_id), Some(sink)) = (self.trace_id, self.sink.as_ref()) else {
            return;
        };
        sink.emit(
            trace_id,
            InspectorEvent::CloneStart {
                operation_id: self.operation_id,
                cache_key: cache_key.to_string(),
                remote: clone_url.to_string(),
            },
        );
    }

    fn on_clone_end(&mut self, cache_key: &str, elapsed: Duration, ok: bool) {
        let (Some(trace_id), Some(sink)) = (self.trace_id, self.sink.as_ref()) else {
            return;
        };
        let outcome = if ok {
            InspectorOutcome::Ok
        } else {
            InspectorOutcome::Network
        };
        sink.emit(
            trace_id,
            InspectorEvent::CloneEnd {
                operation_id: self.operation_id,
                cache_key: cache_key.to_string(),
                end: OpEnd {
                    elapsed_us: to_us(elapsed),
                    result: OutcomeFields::with_outcome(outcome),
                },
            },
        );
    }
}

/// View over a foreign `wit_types::Callout` that exposes the
/// observability classification (`kind`) and redacted `summary`. A
/// newtype is used because orphan rules block adding inherent methods
/// to the bindgen-generated enum.
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
                omnifs_inspector::redact_http_url_for_summary(req.method.as_str(), &req.url)
            },
            wit_types::Callout::FetchBlob(req) => {
                let key = if omnifs_inspector::summary_is_cache_key_shaped(&req.cache_key) {
                    req.cache_key.as_str()
                } else {
                    "redacted"
                };
                format!("blob.fetch {key}")
            },
            wit_types::Callout::GitOpenRepo(req) => {
                format!(
                    "git.open_repo {}",
                    omnifs_inspector::redact_git_remote(&req.clone_url)
                )
            },
            wit_types::Callout::OpenArchive(req) => {
                let strip = req.strip_prefix.as_deref().unwrap_or("");
                format!("archive.open blob={} strip={strip}", req.blob)
            },
            wit_types::Callout::ReadBlob(req) => {
                let len = req.len.unwrap_or(0);
                format!("blob.read {len}B @ {}", req.offset)
            },
        }
    }
}

/// View over `wit_types::CalloutResult` that classifies the result as a
/// stable [`InspectorOutcome`].
struct WitCalloutResultView<'a>(&'a wit_types::CalloutResult);

impl WitCalloutResultView<'_> {
    fn outcome(&self) -> InspectorOutcome {
        match self.0 {
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
            wit_types::CalloutResult::CalloutError(error) => match error.kind {
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
            },
        }
    }
}

/// View over `wit_types::ProviderError` that classifies the error as a
/// stable [`InspectorOutcome`]. Used by `run_op` when a provider returns a
/// typed error.
pub struct WitProviderErrorView<'a>(pub &'a wit_types::ProviderError);

impl WitProviderErrorView<'_> {
    pub fn outcome(&self) -> InspectorOutcome {
        match self.0.kind {
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
}

fn wall_ts() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

// Saturating conversions at the wire boundary. Microsecond elapsed times
// for in-process spans cannot realistically reach u64::MAX (~585k years),
// and callout/result counts cannot reach u32::MAX. Saturation is
// preferred over panic for the few pathological inputs that could occur.
#[allow(clippy::cast_possible_truncation)]
fn to_us(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

#[allow(clippy::cast_possible_truncation)]
fn clamp_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
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

/// Resolve the TCP listen address from env. Empty string disables.
fn live_addr_from_env() -> Option<SocketAddr> {
    let raw = match std::env::var("OMNIFS_INSPECTOR_ADDR").ok() {
        Some(s) if s.is_empty() => return None,
        Some(s) => s,
        None => DEFAULT_LIVE_ADDR.to_string(),
    };
    match raw.parse() {
        Ok(addr) => Some(addr),
        Err(error) => {
            warn!(%error, %raw, "OMNIFS_INSPECTOR_ADDR is not a valid socket address");
            None
        },
    }
}

impl Subscription {
    /// Drive this subscriber: write the history snapshot then forward
    /// future live records as JSONL. Exits on write error, broadcast
    /// closure, or client disconnect. `Lagged` recvs emit a
    /// `# dropped N events` comment line and resume from the most
    /// recent record.
    async fn serve(mut self, mut stream: TcpStream) {
        for record in &self.history {
            if write_record_line(&mut stream, record).await.is_err() {
                return;
            }
        }
        loop {
            match self.live.recv().await {
                Ok(record) => {
                    if write_record_line(&mut stream, &record).await.is_err() {
                        return;
                    }
                },
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    let line = format!("# dropped {n} events\n");
                    if stream.write_all(line.as_bytes()).await.is_err() {
                        return;
                    }
                },
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    }
}

/// Serialize one record to JSON, append a newline, and write to the
/// stream. Returns the write error so the caller can disconnect a
/// dead subscriber. Kept as a free helper rather than a method
/// because the receiver would be the stream (not ours to extend) and
/// the function has no policy worth a wrapper type.
async fn write_record_line(
    stream: &mut TcpStream,
    record: &InspectorRecord,
) -> std::io::Result<()> {
    let mut line = match serialize_record(record) {
        Ok(json) => json,
        Err(error) => {
            warn!(%error, "failed to serialize inspector record");
            return Ok(()); // Skip this record but keep the subscriber.
        },
    };
    line.push('\n');
    stream.write_all(line.as_bytes()).await
}

#[cfg(test)]
impl InspectorSink {
    /// Test-only constructor: no env var dependency, no file tee, no
    /// socket server. Broadcast channel is still constructed so the
    /// emit path exercises both ring + broadcast.
    fn new_for_test(history_cap: usize) -> Self {
        Self::open(InspectorConfig {
            enabled: true,
            history_cap,
            broadcast_cap: DEFAULT_BROADCAST_CAP,
            tee_path: None,
            socket_addr: None,
        })
        .expect("test config is enabled")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;
    use std::thread;

    fn dummy_event() -> InspectorEvent {
        InspectorEvent::FuseStart {
            op: "lookup".into(),
            mount: "test".into(),
            path: "/x".into(),
        }
    }

    #[test]
    fn emit_assigns_monotonic_sequence_ids() {
        let sink = InspectorSink::new_for_test(8);
        for i in 0..5 {
            sink.emit(i, dummy_event());
        }
        let snapshot = sink.history_snapshot();
        let seqs: Vec<u64> = snapshot.iter().map(|r| r.seq).collect();
        assert_eq!(seqs, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn ring_drops_oldest_when_full_and_counts() {
        let sink = InspectorSink::new_for_test(4);
        for i in 0..10 {
            sink.emit(i, dummy_event());
        }
        let snapshot = sink.history_snapshot();
        assert_eq!(
            snapshot.len(),
            4,
            "ring should retain capacity-many records"
        );
        // Retained records are the most recent emissions (seq 7..=10).
        let seqs: Vec<u64> = snapshot.iter().map(|r| r.seq).collect();
        assert_eq!(seqs, vec![7, 8, 9, 10]);
        // Six older records dropped (seq 1..=6).
        assert_eq!(sink.dropped_history(), 6);
    }

    #[tokio::test]
    async fn subscriber_sees_history_snapshot_and_future_events() {
        let sink = Arc::new(InspectorSink::new_for_test(8));
        for i in 0..3 {
            sink.emit(i, dummy_event());
        }
        let mut sub = sink.subscribe();
        // Snapshot reflects the three pre-subscribe emits.
        let snapshot_seqs: Vec<u64> = sub.history.iter().map(|r| r.seq).collect();
        assert_eq!(snapshot_seqs, vec![1, 2, 3]);
        // Future emits arrive on the live receiver.
        sink.emit(99, dummy_event());
        let next = sub.live.recv().await.expect("recv");
        assert_eq!(next.seq, 4);
    }

    #[tokio::test]
    async fn lagged_subscriber_recovers_with_lag_count() {
        // Broadcast capacity is OMNIFS_INSPECTOR_BROADCAST_CAP=256 by
        // default; emit more than that without recving to force Lag.
        // Use a custom small-capacity sink via direct construction.
        let (live, mut rx) = broadcast::channel::<Arc<InspectorRecord>>(4);
        for i in 0u64..10 {
            let record = Arc::new(InspectorRecord::new("t", i, i, dummy_event()).with_seq(i));
            let _ = live.send(record);
        }
        drop(live); // close sender so recv eventually terminates
        let err = rx.recv().await.expect_err("expected lag");
        match err {
            broadcast::error::RecvError::Lagged(n) => assert!(n >= 1),
            broadcast::error::RecvError::Closed => panic!("unexpected: Closed"),
        }
        // After Lag, recv resumes from the most recent record.
        let last = rx.recv().await.expect("recv after lag");
        assert!(last.seq >= 6, "should have advanced past the lag");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn socket_server_writes_snapshot_and_live_records_to_client() {
        use tokio::io::{AsyncBufReadExt, BufReader};

        // Bind an ephemeral port so concurrent tests don't collide.
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");

        let sink = Arc::new(InspectorSink::new_for_test(16));
        // One pre-subscribe emit lands in the history snapshot.
        sink.emit(1, dummy_event());

        let handle = sink.spawn_with_listener(listener, &Handle::current());

        // Connect as a client and read framed JSONL records.
        let stream = TcpStream::connect(&addr).await.expect("client connect");
        let mut reader = BufReader::new(stream);

        // Read the snapshot line.
        let mut snapshot = String::new();
        reader
            .read_line(&mut snapshot)
            .await
            .expect("read snapshot line");
        assert!(snapshot.contains("\"seq\":1"), "got: {snapshot}");

        // Emit one more record and verify it streams to the client.
        sink.emit(2, dummy_event());
        let mut live_line = String::new();
        reader
            .read_line(&mut live_line)
            .await
            .expect("read live line");
        assert!(live_line.contains("\"seq\":2"), "got: {live_line}");

        handle.abort();
    }

    #[test]
    fn emit_under_multithreaded_contention_does_not_deadlock() {
        let sink = Arc::new(InspectorSink::new_for_test(1024));
        let threads = 8;
        let per_thread = 1000;
        let barrier = Arc::new(Barrier::new(threads));
        let mut handles = Vec::new();
        for t in 0..threads {
            let sink = Arc::clone(&sink);
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                barrier.wait();
                for i in 0..per_thread {
                    let tid = u64::try_from(t * per_thread + i).unwrap();
                    sink.emit(tid, dummy_event());
                }
            }));
        }
        for h in handles {
            h.join().expect("thread panicked");
        }
        let total_emitted = sink.next_seq.load(Ordering::Relaxed) - 1;
        assert_eq!(
            total_emitted,
            u64::try_from(threads * per_thread).unwrap(),
            "every emit should have advanced the sequence counter"
        );
    }
}
