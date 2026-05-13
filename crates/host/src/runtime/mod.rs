//! Provider runtime: WASM provider execution and callout handling.
//!
//! Manages the Wasmtime store, routes provider callouts to host
//! implementations (HTTP, Git), and drives async continuations.

pub mod activity;
pub mod archive;
pub mod blob;
mod browse_pipeline;
pub mod capability;
pub mod cloner;
pub mod coverage;
pub mod executor;
pub mod git;
pub mod http_headers;
pub mod inflight;
mod instance;
mod invalidation;
mod log_redaction;
pub mod manifest;
pub mod operation_ids;
pub(crate) mod sandbox;
pub mod tools;
pub mod tree_refs;
pub(crate) mod wasm;

use crate::auth::AuthManager;
use crate::cache;
use crate::cache::blobs::BlobCache;
use crate::cache::{
    AttrPayload, BatchRecord, CacheRecord, DirentRecord, DirentsPayload, EntryMeta, FilePayload,
    Key, LookupPayload, RecordKind, SizeCache,
};
use crate::config::InstanceConfig;
use crate::config::schema;
use crate::omnifs::provider::log::Host as LogHost;
use crate::omnifs::provider::types::{self as wit_types, Host as TypesHost};
use crate::runtime::activity::ActivityTable;
use crate::runtime::archive::ArchiveExecutor;
use crate::runtime::blob::{BlobExecutor, BlobLimits};
use crate::runtime::capability::{CapabilityChecker, CapabilityGrants};
use crate::runtime::cloner::GitCloner;
use crate::runtime::executor::{ErrorKind, HttpExecutor};
use crate::runtime::inflight::InFlight;
use crate::runtime::instance::ProviderInstance;
use crate::runtime::invalidation::InvalidationState;
use crate::runtime::manifest::{DeclaredHandler, read_declared_handlers_from_wasm};
use crate::runtime::operation_ids::OperationIds;
use crate::runtime::tools::archive::ArchiveExtractorComponent;
use crate::runtime::tree_refs::TreeRefs;
use fuser::Notifier;
use log_redaction::{LogUrl, WitHeaders};
use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{Instrument, debug, error, info, trace, warn};
use wasmtime::component::{HasData, ResourceTable};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

const ACTIVITY_TTL: std::time::Duration = std::time::Duration::from_secs(600);

/// FUSE notifier handle (only available on Linux with FUSE support).
pub type NotifierHandle = Arc<Mutex<Option<Notifier>>>;

/// Runtime for one mounted WASM provider component.
///
/// Manages the Wasmtime store, routes callouts, and handles async
/// continuations with operation ID allocation.
pub struct ProviderRuntime {
    instance: ProviderInstance,
    operation_ids: OperationIds,
    http: HttpExecutor,
    git: git::GitExecutor,
    blob: BlobExecutor,
    archive: Arc<ArchiveExecutor>,
    blob_cache: Arc<BlobCache>,
    trees: Arc<TreeRefs>,
    l2: Option<cache::l2::Cache>,
    invalidation: InvalidationState,
    activity_table: Mutex<ActivityTable>,
    declared_handlers: Vec<DeclaredHandler>,
    inflight: InFlight,
}

struct HostState {
    wasi: WasiCtx,
    table: ResourceTable,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl HasData for HostState {
    type Data<'a> = &'a mut HostState;
}

impl TypesHost for HostState {}
impl LogHost for HostState {
    fn log(&mut self, entry: wit_types::LogEntry) {
        match entry.level {
            wit_types::LogLevel::Trace => trace!("{}", entry.message),
            wit_types::LogLevel::Debug => debug!("{}", entry.message),
            wit_types::LogLevel::Info => info!("{}", entry.message),
            wit_types::LogLevel::Warn => warn!("{}", entry.message),
            wit_types::LogLevel::Error => error!("{}", entry.message),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RuntimeBuildError {
    #[error("wasmtime: {0}")]
    Wasmtime(#[from] wasmtime::Error),
    #[error("http client: {0}")]
    HttpClient(#[from] reqwest::Error),
    #[error("invalid config: {0}")]
    InvalidConfig(String),
    #[error("provider protocol: {0}")]
    ProviderProtocol(String),
}

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("wasmtime: {0}")]
    Wasmtime(#[from] wasmtime::Error),
    #[error("provider protocol: {0}")]
    ProviderProtocol(String),
    #[error("provider returned error: {0:?}")]
    ProviderError(wit_types::ProviderError),
    #[error("{op:?} returned unexpected result: {result:?}")]
    UnexpectedOpResult {
        op: Box<Op>,
        result: Box<wit_types::OpResult>,
    },
}

type Result<T> = std::result::Result<T, RuntimeError>;

impl RuntimeError {
    fn unexpected_op_result(op: Op, result: wit_types::OpResult) -> Self {
        Self::UnexpectedOpResult {
            op: Box::new(op),
            result: Box::new(result),
        }
    }
}

impl From<RuntimeError> for RuntimeBuildError {
    fn from(err: RuntimeError) -> Self {
        match err {
            RuntimeError::Wasmtime(e) => Self::Wasmtime(e),
            RuntimeError::ProviderProtocol(msg) => Self::ProviderProtocol(msg),
            RuntimeError::ProviderError(e) => {
                Self::ProviderProtocol(format!("provider error during build: {e:?}"))
            },
            RuntimeError::UnexpectedOpResult { op, result } => Self::ProviderProtocol(format!(
                "{op:?} returned unexpected result during build: {result:?}"
            )),
        }
    }
}

#[derive(Clone, Debug)]
pub enum Op {
    LookupChild {
        parent_path: String,
        name: String,
    },
    ListChildren {
        path: String,
    },
    ReadFile {
        path: String,
    },
    OpenFile {
        path: String,
    },
    ReadChunk {
        handle: u64,
        offset: u64,
        length: u32,
    },
    Initialize,
    OnEvent {
        event: wit_types::ProviderEvent,
    },
}

#[derive(Default)]
struct ProjectionAccumulator {
    dirs: std::collections::BTreeSet<String>,
    children: std::collections::BTreeMap<String, std::collections::BTreeMap<String, DirentRecord>>,
}

impl ProjectionAccumulator {
    fn add(&mut self, entry: &wit_types::ProjEntry, batch: &mut Vec<BatchRecord>) {
        if matches!(entry.kind, wit_types::EntryKind::Directory) {
            self.dirs.insert(entry.path.clone());
        }
        if let Some((parent, name)) = split_projected_path(&entry.path) {
            let name = name.to_string();
            self.children.entry(parent.to_string()).or_default().insert(
                name.clone(),
                DirentRecord {
                    name,
                    meta: EntryMeta::from(&entry.kind),
                },
            );
        }
        ProviderRuntime::push_projected_entry(batch, &entry.path, &entry.kind);
        if let wit_types::EntryKind::File(file) = &entry.kind {
            ProviderRuntime::push_projected_file_content(batch, &entry.path, file);
        }
    }
}

impl ProviderRuntime {
    pub fn new(
        engine: &wasmtime::Engine,
        wasm_path: &Path,
        config: &InstanceConfig,
        cloner: Arc<GitCloner>,
        cache_dir: &Path,
        mount_name: &str,
        extractor: Arc<ArchiveExtractorComponent>,
    ) -> std::result::Result<Self, RuntimeBuildError> {
        let config_bytes = config.config_bytes();
        let instance = ProviderInstance::new(engine, wasm_path, config_bytes)?;

        // Query the provider's declared capabilities and incorporate needs_git.
        let provider_caps = instance.capabilities()?;

        let grants = CapabilityGrants::from_config(config, provider_caps.needs_git);
        let capability = Arc::new(CapabilityChecker::new(grants));

        // Validate instance config against the provider's declared schema.
        let wit_schema = instance.config_schema()?;
        validate_instance_config(wit_schema.as_deref(), config, mount_name)?;
        let auth = if config.auth.is_empty() {
            Arc::new(AuthManager::none())
        } else {
            Arc::new(AuthManager::from_configs(&config.auth).map_err(|e| {
                RuntimeBuildError::ProviderProtocol(format!("auth config error: {e}"))
            })?)
        };

        let trees = Arc::new(TreeRefs::new());
        let git = git::GitExecutor::new(cloner, capability.clone(), trees.clone());

        let provider_cache_root = cache_dir.join("providers").join(mount_name);
        let blob_cache_dir = provider_cache_root.join("blobs");
        let archive_root = provider_cache_root.join("archives");
        if let Err(e) = std::fs::create_dir_all(&blob_cache_dir) {
            warn!(
                dir = %blob_cache_dir.display(),
                error = %e,
                "failed to create blob cache dir; fetch-blob will fail until resolved"
            );
        }
        if let Err(e) = std::fs::create_dir_all(&archive_root) {
            warn!(
                dir = %archive_root.display(),
                error = %e,
                "failed to create archive extract dir; open-archive will fail until resolved"
            );
        }
        let blob_cache = Arc::new(BlobCache::new(blob_cache_dir));
        let archive = Arc::new(ArchiveExecutor::new(
            blob_cache.clone(),
            trees.clone(),
            archive_root,
            extractor,
        ));

        let l2 = {
            let db_path = provider_cache_root.join("browse.redb");
            match cache::l2::Cache::open(&db_path) {
                Ok(cache) => Some(cache),
                Err(e) => {
                    warn!(mount = mount_name, error = %e, "failed to open L2 browse cache");
                    None
                },
            }
        };
        let declared_handlers = read_declared_handlers_from_wasm(wasm_path)
            .map_err(RuntimeBuildError::InvalidConfig)?;

        let blob_limits = BlobLimits::from_config(config);
        let blob = BlobExecutor::new(
            auth.clone(),
            capability.clone(),
            blob_cache.clone(),
            blob_limits,
        )?;
        Ok(Self {
            instance,
            operation_ids: OperationIds::new(),
            http: HttpExecutor::new(auth, capability)?,
            git,
            blob,
            archive,
            blob_cache,
            trees,
            l2,
            invalidation: InvalidationState::default(),
            activity_table: Mutex::new(ActivityTable::new(ACTIVITY_TTL)),
            declared_handlers,
            inflight: InFlight::new(),
        })
    }

    pub fn initialize(&self) -> Result<wit_types::OpResult> {
        let ret = self.instance.initialize()?;
        self.finish_provider_return(&Op::Initialize, ret)
    }

    pub fn shutdown(&self) -> Result<()> {
        self.instance.shutdown()
    }

    pub fn config_schema(&self) -> Result<Option<String>> {
        self.instance.config_schema()
    }

    pub fn capabilities(&self) -> Result<wit_types::RequestedCapabilities> {
        self.instance.capabilities()
    }

    pub fn call_close_file(&self, handle: u64) -> Result<()> {
        self.instance.close_file(handle)
    }

    pub fn cache_get(
        &self,
        path: &str,
        kind: RecordKind,
        aux: Option<&str>,
    ) -> Option<CacheRecord> {
        self.l2
            .as_ref()?
            .get(&Key::with_aux(path, kind, aux))
            .ok()
            .flatten()
    }

    pub fn cache_put(&self, path: &str, kind: RecordKind, aux: Option<&str>, record: &CacheRecord) {
        if let Some(ref l2) = self.l2
            && let Err(e) = l2.put(&Key::with_aux(path, kind, aux), record)
        {
            debug!(path, error = %e, "L2 cache put failed");
        }
    }

    pub fn cache_put_batch(&self, records: &[BatchRecord]) {
        if let Some(ref l2) = self.l2
            && let Err(e) = l2.put_batch(records)
        {
            debug!(error = %e, "L2 cache batch put failed");
        }
    }

    #[doc(hidden)]
    pub fn __active_path_sets(&self) -> Vec<wit_types::ActivePathSet> {
        self.activity_table.lock().active_path_sets()
    }

    pub(super) fn push_projected_file_content(
        batch: &mut Vec<BatchRecord>,
        file_path: &str,
        file: &wit_types::FileProj,
    ) {
        let attrs_cache = cache::FileAttrsCache::from(file);
        if let Some(content) = attrs_cache.inline_bytes()
            && let Some(aux) = attrs_cache.durable_cache_aux()
        {
            let payload = FilePayload::new(attrs_cache.version_token.clone(), content.to_vec());
            if let Some(payload) = payload.serialize() {
                batch.push(BatchRecord::new(
                    file_path,
                    RecordKind::File,
                    aux,
                    CacheRecord::new(RecordKind::File, payload),
                ));
            }
        }
    }

    pub(super) fn push_projected_entry(
        batch: &mut Vec<BatchRecord>,
        path: &str,
        kind: &wit_types::EntryKind,
    ) {
        let meta = EntryMeta::from(kind);
        let lookup = LookupPayload::Positive(meta.clone());
        if let Some(payload) = lookup.serialize() {
            batch.push(BatchRecord::new(
                path,
                RecordKind::Lookup,
                None,
                CacheRecord::new(RecordKind::Lookup, payload),
            ));
        }

        let attr = AttrPayload { meta };
        if let Some(payload) = attr.serialize() {
            batch.push(BatchRecord::new(
                path,
                RecordKind::Attr,
                None,
                CacheRecord::new(RecordKind::Attr, payload),
            ));
        }
    }

    pub(super) fn apply_effects(&self, effects: &[wit_types::Effect]) {
        let mut batch = Vec::new();
        let mut projections = ProjectionAccumulator::default();
        for effect in effects {
            match effect {
                wit_types::Effect::Project(entry) => projections.add(entry, &mut batch),
                wit_types::Effect::InvalidatePath(path) => {
                    self.cache_delete_path(path);
                    self.invalidation.record_path(path.clone());
                },
                wit_types::Effect::InvalidatePrefix(prefix) => {
                    self.cache_delete_prefix(prefix);
                    self.invalidation.record_prefix(prefix.clone());
                },
                wit_types::Effect::DisownTree(_) => {},
            }
        }
        self.merge_projected_dirs(projections, &mut batch);
        if !batch.is_empty() {
            tracing::debug!(target: "omnifs_cache", kind = "project", count = batch.len(), "applying projection effects");
            self.cache_put_batch(&batch);
        }
    }

    fn merge_projected_dirs(
        &self,
        projections: ProjectionAccumulator,
        batch: &mut Vec<BatchRecord>,
    ) {
        let ProjectionAccumulator { dirs, mut children } = projections;
        for dir in dirs {
            let Some(new_children) = children.remove(&dir) else {
                continue;
            };
            let (previously_exhaustive, mut existing) = self
                .cache_get(&dir, RecordKind::Dirents, None)
                .and_then(|record| DirentsPayload::deserialize(&record.payload))
                .map_or_else(
                    || (false, std::collections::BTreeMap::new()),
                    |payload| {
                        (
                            payload.exhaustive,
                            payload
                                .entries
                                .into_iter()
                                .map(|e| (e.name.clone(), e))
                                .collect(),
                        )
                    },
                );
            let introduced = new_children.keys().any(|n| !existing.contains_key(n));
            existing.extend(new_children);
            if let Some(payload) = (DirentsPayload {
                entries: existing.into_values().collect(),
                exhaustive: previously_exhaustive && !introduced,
            })
            .serialize()
            {
                batch.push(BatchRecord::new(
                    dir,
                    RecordKind::Dirents,
                    None,
                    CacheRecord::new(RecordKind::Dirents, payload),
                ));
            }
        }
    }
}

impl ProviderRuntime {
    pub async fn call_timer_tick(&self) -> Result<wit_types::OpResult> {
        let active_paths = self.activity_table.lock().active_path_sets();
        self.run_op(Op::OnEvent {
            event: wit_types::ProviderEvent::TimerTick(wit_types::TimerTickContext {
                active_paths,
            }),
        })
        .await
    }

    pub(super) async fn run_op(&self, op: Op) -> Result<wit_types::OpResult> {
        let id = self.operation_ids.allocate();
        let mut step = self.instance.start_op(&op, id)?;
        loop {
            match step {
                wit_types::ProviderStep::Returned(ret) => {
                    return self.finish_provider_return(&op, ret);
                },
                wit_types::ProviderStep::Suspended(callouts) => {
                    if callouts.is_empty() {
                        return Err(RuntimeError::ProviderProtocol(
                            "provider suspended with no callouts".to_string(),
                        ));
                    }
                    let results = self.dispatch_callouts(id, &callouts).await;
                    step = self.instance.resume(id, results)?;
                },
            }
        }
    }

    /// Runs every callout concurrently and returns positionally aligned
    /// outcomes. The SDK's `join_all` pops outcomes from a FIFO queue in
    /// yield order, so this ordering is load-bearing.
    async fn dispatch_callouts(
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

    fn finish_provider_return(
        &self,
        op: &Op,
        ret: wit_types::ProviderReturn,
    ) -> Result<wit_types::OpResult> {
        Validator::returned(op, &ret, |tree| self.resolve_tree_ref(tree).is_some())
            .map_err(RuntimeError::ProviderProtocol)?;
        self.apply_effects(&ret.effects);
        Ok(ret.result)
    }

    /// Resolve a tree-ref handle to a real filesystem path.
    /// Works for both git clones and extracted archives — they share a
    /// single tree registry, so a `tree-ref` is unambiguous.
    pub fn resolve_tree_ref(&self, tree_ref: u64) -> Option<PathBuf> {
        self.trees.resolve(tree_ref)
    }

    /// Read the full bytes of a stored blob. Used by the FUSE read path
    /// when a `read-file` terminal returns blob-backed file content.
    pub fn read_blob_full(&self, blob_id: u64) -> Result<Vec<u8>> {
        let record = self
            .blob_cache
            .lookup_by_id(blob_id)
            .ok_or_else(|| RuntimeError::ProviderProtocol(format!("blob {blob_id} not found")))?;
        let path = self.blob_cache.blob_path(&record.cache_key);
        std::fs::read(path)
            .map_err(|e| RuntimeError::ProviderProtocol(format!("read blob {blob_id}: {e}")))
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) enum CalloutKind {
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
    pub(super) fn of(callout: &wit_types::Callout) -> Self {
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

    pub(super) fn as_str(self) -> &'static str {
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

pub(super) fn callout_error(
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

pub(super) fn callout_internal(message: impl Into<String>) -> wit_types::CalloutResult {
    callout_error(wit_types::ErrorKind::Internal, message, false)
}
pub(super) fn callout_denied(message: impl Into<String>) -> wit_types::CalloutResult {
    callout_error(wit_types::ErrorKind::Denied, message, false)
}
pub(super) fn callout_not_found(message: impl Into<String>) -> wit_types::CalloutResult {
    callout_error(wit_types::ErrorKind::NotFound, message, false)
}
pub(super) fn callout_too_large(message: impl Into<String>) -> wit_types::CalloutResult {
    callout_error(wit_types::ErrorKind::TooLarge, message, false)
}
pub(super) fn callout_invalid(message: impl Into<String>) -> wit_types::CalloutResult {
    callout_error(wit_types::ErrorKind::InvalidInput, message, false)
}
pub(super) fn callout_network(message: impl Into<String>) -> wit_types::CalloutResult {
    callout_error(wit_types::ErrorKind::Network, message, true)
}

fn unsupported_callout_variant(callout: &wit_types::Callout) -> &'static str {
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
pub(super) fn record_outcome(result: &wit_types::CalloutResult) {
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

#[cfg(test)]
fn validate_operation_result(result: &wit_types::OpResult) -> std::result::Result<(), String> {
    let op = match result {
        wit_types::OpResult::LookupChild(_) => Op::LookupChild {
            parent_path: String::new(),
            name: "child".to_string(),
        },
        wit_types::OpResult::ListChildren(_) => Op::ListChildren {
            path: String::new(),
        },
        wit_types::OpResult::ReadFile(_) => Op::ReadFile {
            path: "file".to_string(),
        },
        wit_types::OpResult::OpenFile(_) => Op::OpenFile {
            path: "file".to_string(),
        },
        wit_types::OpResult::ReadChunk(_) => Op::ReadChunk {
            handle: 0,
            offset: 0,
            length: 0,
        },
        wit_types::OpResult::Initialize(_) => Op::Initialize,
        wit_types::OpResult::OnEvent => Op::OnEvent {
            event: wit_types::ProviderEvent::TimerTick(wit_types::TimerTickContext {
                active_paths: Vec::new(),
            }),
        },
        wit_types::OpResult::Error(_) => Op::Initialize,
        wit_types::OpResult::PlanMutations(_)
        | wit_types::OpResult::Execute(_)
        | wit_types::OpResult::FetchResource(_) => Op::Initialize,
    };
    let ret = wit_types::ProviderReturn {
        result: result.clone(),
        effects: Vec::new(),
    };
    Validator::returned(&op, &ret, |_| true)
}

#[cfg(test)]
fn validate_return(op: &Op, ret: &wit_types::ProviderReturn) -> std::result::Result<(), String> {
    Validator::returned(op, ret, |_| true)
}

fn split_projected_path(path: &str) -> Option<(&str, &str)> {
    let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
    (!name.is_empty()).then_some((parent, name))
}

struct Validator<'a, F> {
    op: &'a Op,
    ret: &'a wit_types::ProviderReturn,
    eager_bytes: usize,
    tree_exists: F,
}

impl<'a, F> Validator<'a, F>
where
    F: Fn(u64) -> bool,
{
    fn returned(
        op: &'a Op,
        ret: &'a wit_types::ProviderReturn,
        tree_exists: F,
    ) -> std::result::Result<(), String> {
        Self {
            op,
            ret,
            eager_bytes: 0,
            tree_exists,
        }
        .validate_return()
    }

    fn validate_return(&mut self) -> std::result::Result<(), String> {
        self.error_returns_do_not_mutate()?;
        self.op_result()?;
        self.effects()?;
        self.subtree_handoff()?;
        Ok(())
    }

    fn error_returns_do_not_mutate(&self) -> std::result::Result<(), String> {
        if matches!(self.ret.result, wit_types::OpResult::Error(_)) && !self.ret.effects.is_empty()
        {
            return Err("provider error returns must not carry effects".to_string());
        }
        Ok(())
    }

    fn effects(&mut self) -> std::result::Result<(), String> {
        for effect in &self.ret.effects {
            match effect {
                wit_types::Effect::Project(entry) => self
                    .entry(&entry.kind)
                    .map_err(|error| format!("project effect {:?}: {error}", entry.path))?,
                wit_types::Effect::InvalidatePath(_) | wit_types::Effect::InvalidatePrefix(_) => {},
                wit_types::Effect::DisownTree(handoff) => {
                    if !(self.tree_exists)(handoff.tree) {
                        return Err(format!(
                            "disown-tree effect for {:?} references unknown tree {}",
                            handoff.path, handoff.tree
                        ));
                    }
                },
            }
        }
        Ok(())
    }

    fn op_result(&mut self) -> std::result::Result<(), String> {
        match (self.op, &self.ret.result) {
            (
                Op::LookupChild { .. },
                wit_types::OpResult::LookupChild(wit_types::LookupChildResult::Entry(entry)),
            ) => {
                self.entry(&entry.target.kind)?;
                for sibling in &entry.siblings {
                    self.entry(&sibling.kind)?;
                }
            },
            (
                Op::LookupChild { .. },
                wit_types::OpResult::LookupChild(
                    wit_types::LookupChildResult::Subtree(_)
                    | wit_types::LookupChildResult::NotFound,
                ),
            )
            | (
                Op::ListChildren { .. },
                wit_types::OpResult::ListChildren(wit_types::ListChildrenResult::Subtree(_)),
            )
            | (Op::ReadChunk { .. }, wit_types::OpResult::ReadChunk(_))
            | (Op::Initialize, wit_types::OpResult::Initialize(_))
            | (Op::OnEvent { .. }, wit_types::OpResult::OnEvent)
            | (_, wit_types::OpResult::Error(_)) => {},
            (
                Op::ListChildren { .. },
                wit_types::OpResult::ListChildren(wit_types::ListChildrenResult::Entries(listing)),
            ) => {
                for entry in &listing.entries {
                    self.entry(&entry.kind)?;
                }
            },
            (Op::ReadFile { .. }, wit_types::OpResult::ReadFile(result)) => {
                self.read_file_result(result)?;
            },
            (Op::OpenFile { .. }, wit_types::OpResult::OpenFile(result)) => {
                Self::file_attrs_metadata(&result.attrs)?;
            },
            _ => {
                return Err(format!(
                    "{:?} returned unexpected result: {:?}",
                    self.op, self.ret.result
                ));
            },
        }
        Ok(())
    }

    fn subtree_handoff(&self) -> std::result::Result<(), String> {
        let handoffs = self.disown_handoffs();
        let subtree = match &self.ret.result {
            wit_types::OpResult::LookupChild(wit_types::LookupChildResult::Subtree(tree))
            | wit_types::OpResult::ListChildren(wit_types::ListChildrenResult::Subtree(tree)) => {
                Some(*tree)
            },
            _ => None,
        };

        match subtree {
            Some(tree) => {
                if handoffs.len() != 1 || handoffs[0].tree != tree {
                    return Err(format!(
                        "subtree result for tree {tree} requires exactly one matching disown-tree effect"
                    ));
                }
                self.validate_handoff_path(tree, handoffs[0])?;
            },
            None if !handoffs.is_empty() => {
                return Err(format!(
                    "disown-tree effects require a subtree result, got {} orphan effect(s)",
                    handoffs.len()
                ));
            },
            None => {},
        }

        Ok(())
    }

    fn validate_handoff_path(
        &self,
        tree: u64,
        handoff: &wit_types::TreeHandoff,
    ) -> std::result::Result<(), String> {
        match self.op {
            Op::LookupChild { parent_path, name } => {
                let expected = if parent_path.is_empty() {
                    name.clone()
                } else {
                    format!("{parent_path}/{name}")
                };
                if handoff.path != expected {
                    return Err(format!(
                        "subtree result for tree {tree} requires disown-tree path {:?}, got {:?}",
                        expected, handoff.path
                    ));
                }
            },
            Op::ListChildren { path } => {
                if handoff.path != *path {
                    return Err(format!(
                        "subtree result for tree {tree} requires disown-tree path {:?}, got {:?}",
                        path, handoff.path
                    ));
                }
            },
            _ => {
                return Err(format!(
                    "subtree result for tree {tree} is not valid for {:?}",
                    self.op
                ));
            },
        }
        Ok(())
    }

    fn disown_handoffs(&self) -> Vec<&wit_types::TreeHandoff> {
        self.ret
            .effects
            .iter()
            .filter_map(|effect| match effect {
                wit_types::Effect::DisownTree(handoff) => Some(handoff),
                _ => None,
            })
            .collect()
    }

    fn entry(&mut self, kind: &wit_types::EntryKind) -> std::result::Result<(), String> {
        match kind {
            wit_types::EntryKind::Directory => Ok(()),
            wit_types::EntryKind::File(file) => self.file_proj(file),
        }
    }

    fn file_proj(&mut self, file: &wit_types::FileProj) -> std::result::Result<(), String> {
        let attrs = cache::FileAttrsCache::from(file);
        attrs.validate()?;
        self.add_eager_bytes(attrs.eager_byte_len())
    }

    fn read_file_result(
        &mut self,
        result: &wit_types::ReadFileResult,
    ) -> std::result::Result<(), String> {
        Self::file_attrs_metadata(&result.attrs)?;
        match &result.bytes {
            wit_types::ReadFileBytes::Inline(bytes) => {
                let attrs = cache::FileAttrsCache::from(&result.attrs);
                attrs
                    .validate_complete_content(bytes.len())
                    .map_err(|error| format!("read-file result: {error}"))?;
                self.add_eager_bytes(bytes.len())?;
            },
            wit_types::ReadFileBytes::Blob(_) => {},
        }
        Ok(())
    }

    fn file_attrs_metadata(attrs: &wit_types::FileAttrs) -> std::result::Result<(), String> {
        if let Some(token) = &attrs.version_token {
            if token.is_empty() {
                return Err("version token must not be empty".to_string());
            }
            if token.len() > cache::MAX_VERSION_TOKEN_BYTES {
                return Err(format!(
                    "version token exceeds {} bytes",
                    cache::MAX_VERSION_TOKEN_BYTES
                ));
            }
        }
        Ok(())
    }

    fn add_eager_bytes(&mut self, bytes: usize) -> std::result::Result<(), String> {
        self.eager_bytes = self
            .eager_bytes
            .checked_add(bytes)
            .ok_or_else(|| "aggregate eager byte count overflowed".to_string())?;
        if self.eager_bytes > cache::MAX_EAGER_RESPONSE_BYTES {
            return Err(format!(
                "terminal response exceeds aggregate eager byte limit of {} bytes",
                cache::MAX_EAGER_RESPONSE_BYTES
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod attr_contract_tests {
    use super::*;

    fn on_event_op() -> Op {
        Op::OnEvent {
            event: wit_types::ProviderEvent::TimerTick(wit_types::TimerTickContext {
                active_paths: Vec::new(),
            }),
        }
    }

    fn lookup_op(parent_path: &str, name: &str) -> Op {
        Op::LookupChild {
            parent_path: parent_path.to_string(),
            name: name.to_string(),
        }
    }

    fn attrs(size: wit_types::FileSize, stability: wit_types::Stability) -> wit_types::FileAttrs {
        wit_types::FileAttrs {
            size,
            stability,
            version_token: None,
        }
    }

    fn file_proj(
        size: wit_types::FileSize,
        bytes: wit_types::ProjBytes,
        stability: wit_types::Stability,
    ) -> wit_types::FileProj {
        wit_types::FileProj {
            attrs: attrs(size, stability),
            bytes,
        }
    }

    fn deferred_exact(size: u64) -> wit_types::FileProj {
        file_proj(
            wit_types::FileSize::Exact(size),
            wit_types::ProjBytes::Deferred(wit_types::ReadMode::Full),
            wit_types::Stability::Immutable,
        )
    }

    #[test]
    fn rejects_invalid_inline_projection_in_entries() {
        let result = wit_types::OpResult::ListChildren(wit_types::ListChildrenResult::Entries(
            wit_types::DirListing {
                entries: vec![wit_types::DirEntry {
                    name: "bad".to_string(),
                    kind: wit_types::EntryKind::File(file_proj(
                        wit_types::FileSize::Unknown,
                        wit_types::ProjBytes::Inline(b"bad".to_vec()),
                        wit_types::Stability::Immutable,
                    )),
                }],
                exhaustive: true,
            },
        ));

        let error = validate_operation_result(&result).unwrap_err();
        assert!(error.contains("inline bytes require Size::Exact"));
    }

    #[test]
    fn rejects_volatile_non_ranged_attrs() {
        let result = wit_types::OpResult::ListChildren(wit_types::ListChildrenResult::Entries(
            wit_types::DirListing {
                entries: vec![wit_types::DirEntry {
                    name: "tail".to_string(),
                    kind: wit_types::EntryKind::File(file_proj(
                        wit_types::FileSize::Unknown,
                        wit_types::ProjBytes::Deferred(wit_types::ReadMode::Full),
                        wit_types::Stability::Volatile,
                    )),
                }],
                exhaustive: true,
            },
        ));

        let error = validate_operation_result(&result).unwrap_err();
        assert!(error.contains("Stability::Volatile requires"));
    }

    #[test]
    fn rejects_bad_project_effect_size_and_aggregate_eager_cap() {
        let mut bad_size_file = deferred_exact(4);
        bad_size_file.bytes = wit_types::ProjBytes::Inline(b"toolong".to_vec());
        let bad_size = wit_types::ProviderReturn {
            result: wit_types::OpResult::OnEvent,
            effects: vec![wit_types::Effect::Project(wit_types::ProjEntry {
                path: "bad".to_string(),
                kind: wit_types::EntryKind::File(bad_size_file),
            })],
        };
        let error = validate_return(&on_event_op(), &bad_size).unwrap_err();
        assert!(error.contains("declares size 4"));

        let too_large = wit_types::ProviderReturn {
            result: wit_types::OpResult::OnEvent,
            effects: (0..9)
                .map(|index| {
                    let bytes = vec![0; cache::MAX_INLINE_PROJECTABLE_BYTES];
                    wit_types::Effect::Project(wit_types::ProjEntry {
                        path: format!("large-{index}"),
                        kind: wit_types::EntryKind::File(wit_types::FileProj {
                            attrs: attrs(
                                wit_types::FileSize::Exact(bytes.len() as u64),
                                wit_types::Stability::Immutable,
                            ),
                            bytes: wit_types::ProjBytes::Inline(bytes),
                        }),
                    })
                })
                .collect(),
        };
        let error = validate_return(&on_event_op(), &too_large).unwrap_err();
        assert!(error.contains("aggregate eager byte limit"));
    }

    #[test]
    fn rejects_read_content_that_violates_declared_size() {
        let result = wit_types::OpResult::ReadFile(wit_types::ReadFileResult {
            attrs: attrs(
                wit_types::FileSize::NonZero,
                wit_types::Stability::Immutable,
            ),
            bytes: wit_types::ReadFileBytes::Inline(Vec::new()),
        });

        let error = validate_operation_result(&result).unwrap_err();
        assert!(error.contains("read-file result"));
        assert!(error.contains("Size::NonZero"));
    }

    #[test]
    fn rejects_empty_version_tokens() {
        let mut file = deferred_exact(1);
        file.attrs.version_token = Some(String::new());
        let result = wit_types::OpResult::ListChildren(wit_types::ListChildrenResult::Entries(
            wit_types::DirListing {
                entries: vec![wit_types::DirEntry {
                    name: "versioned".to_string(),
                    kind: wit_types::EntryKind::File(file),
                }],
                exhaustive: true,
            },
        ));

        let error = validate_operation_result(&result).unwrap_err();
        assert!(error.contains("version token must not be empty"));
    }

    #[test]
    fn subtree_results_require_matching_disown_effect() {
        let missing = wit_types::ProviderReturn {
            result: wit_types::OpResult::LookupChild(wit_types::LookupChildResult::Subtree(7)),
            effects: Vec::new(),
        };
        let error = validate_return(&lookup_op("", "checkout"), &missing).unwrap_err();
        assert!(error.contains("requires exactly one matching disown-tree effect"));

        let valid = wit_types::ProviderReturn {
            result: wit_types::OpResult::LookupChild(wit_types::LookupChildResult::Subtree(7)),
            effects: vec![wit_types::Effect::DisownTree(wit_types::TreeHandoff {
                path: "checkout".to_string(),
                tree: 7,
            })],
        };
        validate_return(&lookup_op("", "checkout"), &valid).unwrap();

        let error = validate_return(&lookup_op("", "other"), &valid).unwrap_err();
        assert!(error.contains("requires disown-tree path"));

        let orphan = wit_types::ProviderReturn {
            result: wit_types::OpResult::OnEvent,
            effects: vec![wit_types::Effect::DisownTree(wit_types::TreeHandoff {
                path: "checkout".to_string(),
                tree: 7,
            })],
        };
        let error = validate_return(&on_event_op(), &orphan).unwrap_err();
        assert!(error.contains("require a subtree result"));
    }

    #[test]
    fn error_returns_reject_effects() {
        let ret = wit_types::ProviderReturn {
            result: wit_types::OpResult::Error(wit_types::ProviderError {
                kind: wit_types::ErrorKind::Internal,
                message: "failed".to_string(),
                retryable: false,
            }),
            effects: vec![wit_types::Effect::InvalidatePath("x".to_string())],
        };

        let error = validate_return(&on_event_op(), &ret).unwrap_err();
        assert!(error.contains("error returns must not carry effects"));
    }
}

fn validate_instance_config(
    schema_json: Option<&str>,
    config: &InstanceConfig,
    mount_name: &str,
) -> std::result::Result<(), RuntimeBuildError> {
    let Some(schema_json) = schema_json else {
        return Ok(());
    };

    let empty_config = serde_json::Value::Object(serde_json::Map::new());
    let config_value = config.config_raw.as_ref().unwrap_or(&empty_config);
    match schema::validate_config(schema_json, config_value) {
        Ok(()) => Ok(()),
        Err(schema::SchemaError::Validation(error)) => Err(RuntimeBuildError::InvalidConfig(
            format!("config for mount {mount_name} failed validation: {error}"),
        )),
        Err(schema::SchemaError::InvalidSchema(error)) => Err(RuntimeBuildError::ProviderProtocol(
            format!("provider config schema for mount {mount_name} is invalid: {error}"),
        )),
    }
}

impl From<&wit_types::FileProj> for cache::FileAttrsCache {
    fn from(file: &wit_types::FileProj) -> Self {
        Self {
            size: SizeCache::from(&file.attrs.size),
            bytes: cache::BytesCache::from(&file.bytes),
            stability: cache::StabilityCache::from(file.attrs.stability),
            version_token: file.attrs.version_token.clone(),
        }
    }
}

impl From<&wit_types::FileAttrs> for cache::FileAttrsCache {
    fn from(attrs: &wit_types::FileAttrs) -> Self {
        Self {
            size: SizeCache::from(&attrs.size),
            bytes: cache::BytesCache::Deferred(cache::ReadModeCache::Full),
            stability: cache::StabilityCache::from(attrs.stability),
            version_token: attrs.version_token.clone(),
        }
    }
}

impl From<&wit_types::FileSize> for SizeCache {
    fn from(size: &wit_types::FileSize) -> Self {
        match size {
            wit_types::FileSize::Exact(size) => Self::Exact(*size),
            wit_types::FileSize::NonZero => Self::NonZero,
            wit_types::FileSize::Unknown => Self::Unknown,
        }
    }
}

impl From<&wit_types::ProjBytes> for cache::BytesCache {
    fn from(bytes: &wit_types::ProjBytes) -> Self {
        match bytes {
            wit_types::ProjBytes::Inline(bytes) => Self::Inline(bytes.clone()),
            wit_types::ProjBytes::Deferred(mode) => {
                Self::Deferred(cache::ReadModeCache::from(*mode))
            },
        }
    }
}

impl From<wit_types::ReadMode> for cache::ReadModeCache {
    fn from(mode: wit_types::ReadMode) -> Self {
        match mode {
            wit_types::ReadMode::Full => Self::Full,
            wit_types::ReadMode::Ranged => Self::Ranged,
        }
    }
}

impl From<wit_types::Stability> for cache::StabilityCache {
    fn from(stability: wit_types::Stability) -> Self {
        match stability {
            wit_types::Stability::Immutable => Self::Immutable,
            wit_types::Stability::Mutable => Self::Mutable,
            wit_types::Stability::Volatile => Self::Volatile,
        }
    }
}

impl From<&wit_types::EntryKind> for EntryMeta {
    fn from(kind: &wit_types::EntryKind) -> Self {
        match kind {
            wit_types::EntryKind::Directory => Self::directory(),
            wit_types::EntryKind::File(file) => Self::file(cache::FileAttrsCache::from(file)),
        }
    }
}

impl From<&wit_types::EntryKind> for cache::EntryKindCache {
    fn from(kind: &wit_types::EntryKind) -> Self {
        match kind {
            wit_types::EntryKind::Directory => Self::Directory,
            wit_types::EntryKind::File(_) => Self::File,
        }
    }
}

impl From<ErrorKind> for wit_types::ErrorKind {
    fn from(kind: ErrorKind) -> Self {
        match kind {
            ErrorKind::Network => Self::Network,
            ErrorKind::Timeout => Self::Timeout,
            ErrorKind::Denied => Self::Denied,
            ErrorKind::NotFound => Self::NotFound,
            ErrorKind::RateLimited => Self::RateLimited,
            ErrorKind::InvalidInput => Self::InvalidInput,
            ErrorKind::TooLarge => Self::TooLarge,
            ErrorKind::Internal => Self::Internal,
        }
    }
}

fn absolute_mount_path(path: &str) -> String {
    if path.is_empty() {
        "/".to_string()
    } else if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    }
}

/// Test-only re-exports used by the `callout_tracing` integration test
/// to drive canned futures/results through the same instrumentation
/// pattern as `dispatch_one` without spinning up real executors.
#[doc(hidden)]
pub mod __test_support {
    use super::{
        LogUrl as InternalLogUrl, WitHeaders as InternalWitHeaders, record_outcome as inner_record,
    };
    use crate::omnifs::provider::types as wit_types;
    use std::fmt;

    /// Stable kind labels used by the outer dispatch span. Kept in lockstep
    /// with the internal `CalloutKind::as_str()` values.
    pub fn kind_label(callout: &wit_types::Callout) -> &'static str {
        super::CalloutKind::of(callout).as_str()
    }

    /// Public re-display wrapper for redacting URLs in log output.
    pub struct LogUrl<'a>(pub &'a str);
    impl fmt::Display for LogUrl<'_> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            InternalLogUrl(self.0).fmt(f)
        }
    }

    /// Public re-display wrapper for redacting WIT headers in log output.
    pub struct WitHeaders<'a>(pub &'a [wit_types::Header]);
    impl fmt::Display for WitHeaders<'_> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            InternalWitHeaders(self.0).fmt(f)
        }
    }

    /// Records the outcome fields on `Span::current()` for the given
    /// callout result, exactly as the production executor methods do.
    pub fn record_outcome(result: &wit_types::CalloutResult) {
        inner_record(result);
    }

    /// Returns the variant label for an unsupported callout, mirroring
    /// the production `unsupported_callout_variant` used by the inner
    /// instrumented helper.
    pub fn unsupported_callout_variant(callout: &wit_types::Callout) -> &'static str {
        super::unsupported_callout_variant(callout)
    }
}
