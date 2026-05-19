//! Provider runtime: WASM provider execution and callout handling.
//!
//! Manages the Wasmtime store, routes provider callouts to host
//! implementations (HTTP, Git), and drives async continuations.

pub mod activity;
pub mod archive;
pub mod blob;
mod browse_pipeline;
pub(crate) mod callouts;
pub mod capability;
pub mod cloner;
pub mod coverage;
pub(crate) mod effects;
pub mod git;
pub mod http_headers;
pub mod http_stack;
pub mod inflight;
mod instance;
mod invalidation;
pub(crate) mod log_redaction;
pub mod manifest;
pub(super) mod op;
pub mod operation_ids;
pub(crate) mod sandbox;
pub mod tools;
pub mod tree_refs;
pub(crate) mod wasm;
pub(super) mod wit_conversions;

use crate::auth::AuthManager;
use crate::cache;
use crate::cache::blobs::BlobCache;
use crate::cache::{BatchRecord, CacheRecord, Key, RecordKind};
use crate::config::InstanceConfig;
use crate::config::schema;
use crate::omnifs::provider::log::Host as LogHost;
use crate::omnifs::provider::types::{self as wit_types, Host as TypesHost};
use crate::runtime::activity::ActivityTable;
use crate::runtime::archive::ArchiveExecutor;
use crate::runtime::blob::{BlobExecutor, BlobLimits};
use crate::runtime::capability::{CapabilityChecker, CapabilityGrants};
use crate::runtime::cloner::GitCloner;
use crate::runtime::http_stack::HttpStack;
use crate::runtime::inflight::InFlight;
use crate::runtime::instance::ProviderInstance;
use crate::runtime::invalidation::InvalidationState;
use crate::runtime::manifest::{DeclaredHandler, read_declared_handlers_from_wasm};
use crate::runtime::operation_ids::OperationIds;
use crate::runtime::tools::archive::ArchiveExtractorComponent;
use crate::runtime::tree_refs::TreeRefs;
use fuser::Notifier;
use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{debug, error, info, trace, warn};
use wasmtime::component::{HasData, ResourceTable};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

pub use op::Op;
use op::Validator;

const ACTIVITY_TTL: std::time::Duration = std::time::Duration::from_secs(600);
const HTTP_FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// FUSE notifier handle (only available on Linux with FUSE support).
pub type NotifierHandle = Arc<Mutex<Option<Notifier>>>;

/// Runtime for one mounted WASM provider component.
///
/// Manages the Wasmtime store, routes callouts, and handles async
/// continuations with operation ID allocation.
pub struct ProviderRuntime {
    instance: ProviderInstance,
    operation_ids: OperationIds,
    http: Arc<HttpStack>,
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
        let preopens = config
            .capabilities
            .as_ref()
            .and_then(|c| c.preopened_paths.as_deref())
            .unwrap_or(&[]);
        let instance = ProviderInstance::new(engine, wasm_path, config_bytes, preopens)?;

        // Query the provider's declared capabilities and incorporate needs_git.
        let provider_caps = instance.capabilities()?;

        let grants = CapabilityGrants::from_config(config, &provider_caps);
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
        let http = Arc::new(HttpStack::new(auth.clone(), capability.clone())?);
        let blob = BlobExecutor::new(Arc::clone(&http), blob_cache.clone(), blob_limits);
        Ok(Self {
            instance,
            operation_ids: OperationIds::new(),
            http,
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
    use crate::omnifs::provider::types as wit_types;
    use crate::runtime::callouts::{
        CalloutKind, record_outcome as inner_record,
        unsupported_callout_variant as inner_unsupported_variant,
    };
    use crate::runtime::log_redaction::{
        LogUrl as InternalLogUrl, WitHeaders as InternalWitHeaders,
    };
    use std::fmt;

    /// Stable kind labels used by the outer dispatch span. Kept in lockstep
    /// with the internal `CalloutKind::as_str()` values.
    pub fn kind_label(callout: &wit_types::Callout) -> &'static str {
        CalloutKind::of(callout).as_str()
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
        inner_unsupported_variant(callout)
    }
}
