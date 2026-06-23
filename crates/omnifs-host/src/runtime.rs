//! Engine/instance/mount lifecycle for one WASM provider.
//!
//! `Runtime` manages the Wasmtime store lifetime, provider initialization,
//! executor handles (HTTP, Git, Blob, Archive), and cache/mount lifecycle.
//! Op execution is in `op_lifecycle`; WASI store plumbing is in `wasi`.

use crate::archive::ArchiveExecutor;
use crate::auth::{AuthManager, credential_store_for_file};
use crate::blob::{BlobExecutor, BlobLimits};
use crate::blob_cache::BlobCache;
use crate::capability::CapabilityChecker;
use crate::cloner::GitCloner;
use crate::git;
use crate::http::HttpStack;
use crate::inflight::InFlight;
use crate::inspector::{self, InspectorSink};
use crate::instance::Instance;
use crate::invalidation::InvalidationState;
use crate::manifest::Artifact;
use crate::tools::archive::ArchiveExtractorComponent;
use crate::tree_refs::TreeRefs;
use dashmap::DashMap;
use omnifs_cache::{Caches, Store};
use omnifs_caps::Grant;
use omnifs_core::ProviderId;
use omnifs_core::path::Path;
use omnifs_mount::ProviderConfig;
use omnifs_mount::mounts::{ProviderStore, Resolved};
use omnifs_wit::provider::types as wit_types;

use std::io;
use std::path::{Path as StdPath, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::clock::{self, DYNAMIC_TTL_MILLIS};
use crate::object_id::ObjectId;
use crate::op::Op;
use crate::op_validate;

pub(crate) const HTTP_FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
// Host-side 429 cooldown when the provider error carried no Retry-After.
const RATE_LIMIT_DEFAULT_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(5);
// Upper bound so a hostile Retry-After cannot overflow `Instant` or wedge the
// window open indefinitely.
const RATE_LIMIT_MAX_COOLDOWN: std::time::Duration = std::time::Duration::from_hours(1);
const PROVIDER_CACHE_SUBDIR: &str = "providers";
const BLOB_CACHE_SUBDIR: &str = "blobs";
const ARCHIVE_CACHE_SUBDIR: &str = "archives";

/// Host-owned filesystem context for provider runtime and mount lifecycle.
#[derive(Clone, Debug)]
pub struct HostContext {
    cache_dir: PathBuf,
    config_dir: PathBuf,
    providers_dir: PathBuf,
    credentials_file: PathBuf,
}

impl HostContext {
    pub fn new(
        cache_dir: impl AsRef<StdPath>,
        config_dir: impl AsRef<StdPath>,
        providers_dir: impl AsRef<StdPath>,
        credentials_file: impl AsRef<StdPath>,
    ) -> Self {
        Self {
            cache_dir: cache_dir.as_ref().to_path_buf(),
            config_dir: config_dir.as_ref().to_path_buf(),
            providers_dir: providers_dir.as_ref().to_path_buf(),
            credentials_file: credentials_file.as_ref().to_path_buf(),
        }
    }

    pub fn cache_dir(&self) -> &StdPath {
        &self.cache_dir
    }

    pub fn config_dir(&self) -> &StdPath {
        &self.config_dir
    }

    pub fn providers_dir(&self) -> &StdPath {
        &self.providers_dir
    }

    pub fn credentials_file(&self) -> &StdPath {
        &self.credentials_file
    }

    pub(crate) fn mounts_dir(&self) -> PathBuf {
        self.config_dir.join(omnifs_home::MOUNTS_SUBDIR)
    }

    pub(crate) fn wasm_cache_dir(&self) -> PathBuf {
        self.cache_dir.join("wasm")
    }

    /// `by-hash/<hex>.wasm` for a pinned provider id: the serving path the
    /// daemon loads. Identity is the content hash, never a filename.
    pub(crate) fn provider_path_by_id(&self, id: &ProviderId) -> PathBuf {
        ProviderStore::new(&self.providers_dir).by_hash_path(id)
    }

    /// Host-internal tools (the archive extractor) live flat in `providers_dir`,
    /// not in the content-addressed `by-hash/` store.
    pub(crate) fn archive_tool_path(&self, file: &str) -> PathBuf {
        self.providers_dir.join(file)
    }
}

/// Runtime for one mounted WASM provider component.
///
/// Manages the Wasmtime store, routes callouts, and handles async
/// continuations with operation ID allocation.
pub struct Runtime {
    pub(crate) instance: Instance,
    initialize_result: wit_types::InitializeResult,
    pub(crate) mount_name: String,
    pub(crate) provider_name: String,
    next_operation_id: AtomicU64,
    pub(crate) http: Arc<HttpStack>,
    pub(crate) git: git::GitExecutor,
    pub(crate) blob: BlobExecutor,
    pub(crate) archive: Arc<ArchiveExecutor>,
    blob_cache: Arc<BlobCache>,
    trees: Arc<TreeRefs>,
    pub(crate) cache: Store,
    pub(crate) invalidation: InvalidationState,
    pub(crate) inflight: InFlight,
    /// Per-path locks serializing the read-modify-write of a paged
    /// directory's accumulated dirents. Two concurrent `@next` (or `@all`)
    /// reads on the same directory must not both snapshot the same base and
    /// each append their page, which would lose a page. Held across the
    /// continuation fetch in [`paginate_next`](Runtime::paginate_next).
    pub(crate) pagination_locks: DashMap<String, Arc<tokio::sync::Mutex<()>>>,
    // Per-mount rate-limit window. `Some(open_until)` while the mount's
    // provider is throttled (set from a 429's Retry-After); reads serve stale
    // cache instead of EAGAIN until it clears. std Mutex: the critical section
    // is a single set/get with no await held across it.
    rate_limit_until: std::sync::Mutex<Option<std::time::Instant>>,
    /// Injected inspector sink. Defaults to the process-global configured sink
    /// so production wiring is unchanged; tests can supply their own.
    pub(crate) inspector: Option<Arc<InspectorSink>>,
}

pub struct TestOp<'a> {
    runtime: &'a Runtime,
    op: Op,
    id: u64,
    op_gen: u64,
    state: TestOpState,
}

enum TestOpState {
    Suspended(Vec<wit_types::Callout>),
    Returned {
        result: Box<wit_types::OpResult>,
        effects: Box<wit_types::Effects>,
    },
}

pub struct Namespace<'a> {
    pub(crate) runtime: &'a Runtime,
}

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("wasmtime: {0}")]
    Wasmtime(#[from] wasmtime::Error),
    #[error("http client: {0}")]
    HttpClient(#[from] reqwest::Error),
    #[error("invalid config: {0}")]
    InvalidConfig(String),
    #[error("cache dir {path}: {source}")]
    CacheDir { path: PathBuf, source: io::Error },
    #[error("provider protocol: {0}")]
    ProviderProtocol(String),
}

#[derive(Debug)]
struct CacheDirs {
    blob: PathBuf,
    archive_root: PathBuf,
}

impl CacheDirs {
    fn prepare(cache_dir: &StdPath, mount_name: &str) -> std::result::Result<Self, BuildError> {
        let provider_root = Self::provider_root(cache_dir, mount_name);
        let dirs = Self {
            blob: provider_root.join(BLOB_CACHE_SUBDIR),
            archive_root: provider_root.join(ARCHIVE_CACHE_SUBDIR),
        };
        dirs.prepare_all()?;
        Ok(dirs)
    }

    fn provider_root(cache_dir: &StdPath, mount_name: &str) -> PathBuf {
        cache_dir.join(PROVIDER_CACHE_SUBDIR).join(mount_name)
    }

    #[cfg(test)]
    fn blob_path(cache_dir: &StdPath, mount_name: &str) -> PathBuf {
        Self::provider_root(cache_dir, mount_name).join(BLOB_CACHE_SUBDIR)
    }

    fn prepare_all(&self) -> std::result::Result<(), BuildError> {
        [&self.blob, &self.archive_root]
            .into_iter()
            .try_for_each(|path| {
                std::fs::create_dir_all(path).map_err(|source| BuildError::CacheDir {
                    path: path.clone(),
                    source,
                })
            })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
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

pub(crate) type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub(crate) fn unexpected_op_result(op: Op, result: wit_types::OpResult) -> Self {
        Self::UnexpectedOpResult {
            op: Box::new(op),
            result: Box::new(result),
        }
    }
}

impl From<Error> for BuildError {
    fn from(err: Error) -> Self {
        match err {
            Error::Wasmtime(e) => Self::Wasmtime(e),
            Error::ProviderProtocol(msg) => Self::ProviderProtocol(msg),
            Error::ProviderError(e) => {
                Self::ProviderProtocol(format!("provider error during build: {e:?}"))
            },
            Error::UnexpectedOpResult { op, result } => Self::ProviderProtocol(format!(
                "{op:?} returned unexpected result during build: {result:?}"
            )),
        }
    }
}

impl Runtime {
    #[must_use]
    pub fn mount_name(&self) -> &str {
        &self.mount_name
    }

    pub fn provider_name(&self) -> &str {
        &self.provider_name
    }

    pub fn namespace(&self) -> Namespace<'_> {
        Namespace { runtime: self }
    }

    pub fn new(
        engine: &wasmtime::Engine,
        wasm_path: &StdPath,
        config: &Resolved,
        cloner: Arc<GitCloner>,
        context: &HostContext,
        extractor: Arc<ArchiveExtractorComponent>,
        caches: &Arc<Caches>,
    ) -> std::result::Result<Self, BuildError> {
        let mount_name = config.spec.mount.as_str();
        let config_bytes = config.config_bytes();
        let preopens = config
            .spec
            .capabilities
            .as_ref()
            .and_then(|c| c.preopened_paths.as_ref())
            .map_or(&[][..], Grant::literal);
        // Load the pinned artifact's manifest once: capability/config-schema
        // validation and auth both read it, and enforcement rests on this
        // pinned manifest, never on a spec-stamped snapshot.
        let manifest = Artifact::load(wasm_path)
            .and_then(|artifact| artifact.metadata())
            .map_err(BuildError::InvalidConfig)?;

        let instance = Instance::new(engine, wasm_path, config_bytes, preopens)?;

        // `schemars::Schema::as_value` is the method clippy would name here, but
        // schemars is not a direct dependency, so the closure stays.
        #[allow(clippy::redundant_closure_for_method_calls)]
        let config_schema = manifest
            .as_ref()
            .and_then(|manifest| manifest.config_schema.as_ref())
            .map(|schema| schema.as_value());
        validate_instance_config(config_schema, config, mount_name)?;

        let init_return = instance.initialize().map_err(BuildError::from)?;
        let initialize_result = finish_initialize_return(init_return)?;
        let capability = Arc::new(CapabilityChecker::from_config(
            config,
            &initialize_result.capabilities,
        ));

        let auth_manifest = manifest
            .as_ref()
            .and_then(omnifs_provider::ProviderManifest::wasm_auth_manifest);
        let auth = if config.spec.auth.is_empty() {
            Arc::new(AuthManager::none())
        } else {
            let store = credential_store_for_file(context.credentials_file());
            Arc::new(
                AuthManager::from_configs_manifest_store_with_store(
                    &config.spec.auth,
                    auth_manifest.as_ref(),
                    &config.provider_name,
                    store,
                )
                .map_err(|e| BuildError::ProviderProtocol(format!("auth config error: {e}")))?,
            )
        };

        let trees = Arc::new(TreeRefs::new());
        let git = git::GitExecutor::new(cloner, capability.clone(), trees.clone());

        let cache_dirs = CacheDirs::prepare(context.cache_dir(), mount_name)?;
        let blob_cache = Arc::new(BlobCache::new(cache_dirs.blob));
        let archive = Arc::new(ArchiveExecutor::new(
            blob_cache.clone(),
            trees.clone(),
            cache_dirs.archive_root,
            extractor,
        ));

        // Per-mount facade: scopes all cache keys with "{mount}\x1f".
        let cache = caches.mount(mount_name);
        let blob_limits = BlobLimits::from_config(config);
        let http = Arc::new(HttpStack::new(auth.clone(), capability.clone())?);
        let blob = BlobExecutor::new(Arc::clone(&http), blob_cache.clone(), blob_limits);
        Ok(Self {
            instance,
            initialize_result,
            mount_name: mount_name.to_string(),
            provider_name: config.provider_name.clone(),
            next_operation_id: AtomicU64::new(1),
            http,
            git,
            blob,
            archive,
            blob_cache,
            trees,
            cache,
            invalidation: InvalidationState::default(),
            inflight: InFlight::new(),
            pagination_locks: DashMap::new(),
            rate_limit_until: std::sync::Mutex::new(None),
            inspector: inspector::global(),
        })
    }

    pub fn shutdown(&self) -> Result<()> {
        self.instance.shutdown()
    }

    #[must_use]
    pub fn requested_capabilities(&self) -> &wit_types::RequestedCapabilities {
        &self.initialize_result.capabilities
    }

    #[must_use]
    pub fn provider_info(&self) -> &wit_types::ProviderInfo {
        &self.initialize_result.info
    }

    pub fn call_close_file(&self, handle: u64) -> Result<()> {
        self.instance.close_file(handle)
    }

    pub fn cache(&self) -> &Store {
        &self.cache
    }

    /// Test-only entry to drive provider effects from FUSE-path pagination
    /// harnesses without routing through a provider component.
    #[doc(hidden)]
    pub fn apply_effects_for_test(&self, effects: &wit_types::Effects, op_gen: u64) {
        let now = clock::now_millis();
        let (prefixes, paths) =
            crate::materialize::Materializer::new(&self.cache).apply(effects, op_gen, now);
        self.record_view_invalidations(prefixes, paths);
    }

    #[doc(hidden)]
    pub fn apply_not_found_negative(
        &self,
        path: &Path,
        maybe_id: Option<&wit_types::LogicalId>,
        op_gen: u64,
        now_millis: u64,
    ) {
        let id_bytes = maybe_id.map(|id| ObjectId::from_wit(id).as_bytes().to_vec());
        self.cache.put_negative(
            path,
            id_bytes.as_deref(),
            op_gen,
            DYNAMIC_TTL_MILLIS,
            now_millis,
        );
    }

    /// Arm the mount's rate-limit window after a 429. `retry_after` is the
    /// provider error's structured Retry-After (seconds) if present.
    pub(crate) fn note_rate_limited(&self, retry_after: Option<std::time::Duration>) {
        let cooldown = retry_after
            .unwrap_or(RATE_LIMIT_DEFAULT_COOLDOWN)
            .min(RATE_LIMIT_MAX_COOLDOWN);
        let until = std::time::Instant::now() + cooldown;
        *self.rate_limit_until.lock().unwrap() = Some(until);
    }

    /// The instant the mount's rate-limit window closes, if currently open.
    /// Lazily clears an expired window.
    pub fn rate_limited_until(&self) -> Option<std::time::Instant> {
        let mut guard = self.rate_limit_until.lock().unwrap();
        match *guard {
            Some(until) if until > std::time::Instant::now() => Some(until),
            Some(_) => {
                *guard = None;
                None
            },
            None => None,
        }
    }

    pub(crate) fn next_operation_id(&self) -> u64 {
        self.next_operation_id.fetch_add(1, Ordering::Relaxed)
    }
}

impl Runtime {
    pub fn start_op(&self, op: Op) -> Result<TestOp<'_>> {
        let op_gen = self.cache.current_generation();
        let id = self.next_operation_id();
        let step = self.instance.start_op(&op, id)?;
        TestOp::from_step(self, op, id, op_gen, step)
    }

    pub async fn call_timer_tick(&self) -> Result<wit_types::OpResult> {
        self.run_op(
            Op::OnEvent {
                event: wit_types::ProviderEvent::TimerTick,
            },
            None,
        )
        .await
    }

    /// Resolve a tree-ref handle to a real filesystem path.
    /// Works for both git clones and extracted archives — they share a
    /// single tree registry, so a `tree-ref` is unambiguous.
    pub fn resolve_tree_ref(&self, tree_ref: u64) -> Option<PathBuf> {
        self.trees.resolve(tree_ref)
    }

    /// Serve the canonical bytes for `path` from the anchor-keyed object
    /// cache. Used by the FUSE read path when a `read-file` terminal answers
    /// `byte-source::canonical`: the host resolves the longest covering
    /// anchor and returns those bytes without copying across the WIT
    /// (ADR-0001 §5.1). `None` when no stored anchor covers `path`.
    pub fn canonical_bytes_for(&self, path: &Path) -> Option<Vec<u8>> {
        self.cache
            .cached_canonical_for(path)
            .map(|canonical| canonical.bytes)
    }

    /// Read the full bytes of a stored blob. Used by the FUSE read path
    /// when a `read-file` terminal returns blob-backed file content.
    pub fn read_blob_full(&self, blob_id: u64) -> Result<Vec<u8>> {
        let record = self
            .blob_cache
            .lookup_by_id(blob_id)
            .ok_or_else(|| Error::ProviderProtocol(format!("blob {blob_id} not found")))?;
        let path = self.blob_cache.blob_path(&record.cache_key);
        std::fs::read(path)
            .map_err(|e| Error::ProviderProtocol(format!("read blob {blob_id}: {e}")))
    }
}

impl<'a> TestOp<'a> {
    fn from_step(
        runtime: &'a Runtime,
        op: Op,
        id: u64,
        op_gen: u64,
        step: wit_types::ProviderStep,
    ) -> Result<Self> {
        let mut started = Self {
            runtime,
            op,
            id,
            op_gen,
            state: TestOpState::Suspended(Vec::new()),
        };
        started.state = started.state_from_step(step)?;
        Ok(started)
    }

    fn state_from_step(&self, step: wit_types::ProviderStep) -> Result<TestOpState> {
        match step {
            wit_types::ProviderStep::Suspended(callouts) => {
                if callouts.is_empty() {
                    return Err(Error::ProviderProtocol(
                        "provider suspended with no callouts".to_string(),
                    ));
                }
                Ok(TestOpState::Suspended(callouts))
            },
            wit_types::ProviderStep::Returned(ret) => {
                let effects = ret.effects.clone();
                let result = self
                    .runtime
                    .finish_provider_return(&self.op, ret, self.op_gen)?;
                self.runtime.note_returned_result(&result);
                Ok(TestOpState::Returned {
                    result: Box::new(result),
                    effects: Box::new(effects),
                })
            },
        }
    }

    pub fn callouts(&self) -> &[wit_types::Callout] {
        match &self.state {
            TestOpState::Suspended(callouts) => callouts,
            TestOpState::Returned { .. } => &[],
        }
    }

    pub fn is_suspended(&self) -> bool {
        matches!(self.state, TestOpState::Suspended(_))
    }

    pub fn is_returned(&self) -> bool {
        matches!(self.state, TestOpState::Returned { .. })
    }

    #[allow(clippy::needless_pass_by_value)]
    pub fn resume(&mut self, results: Vec<wit_types::CalloutResult>) -> Result<()> {
        if !self.is_suspended() {
            return Err(Error::ProviderProtocol(
                "cannot resume an operation that has already returned".to_string(),
            ));
        }
        let step = self.runtime.instance.resume(self.id, results)?;
        self.state = self.state_from_step(step)?;
        Ok(())
    }

    pub fn into_result(self) -> Result<wit_types::OpResult> {
        match self.state {
            TestOpState::Returned { result, .. } => Ok(*result),
            TestOpState::Suspended(_) => Err(Error::ProviderProtocol(
                "operation is still suspended".to_string(),
            )),
        }
    }

    pub fn result(&self) -> Option<&wit_types::OpResult> {
        match &self.state {
            TestOpState::Returned { result, .. } => Some(result.as_ref()),
            TestOpState::Suspended(_) => None,
        }
    }

    pub fn effects(&self) -> Option<&wit_types::Effects> {
        match &self.state {
            TestOpState::Returned { effects, .. } => Some(effects.as_ref()),
            TestOpState::Suspended(_) => None,
        }
    }

    pub fn into_result_and_effects(self) -> Result<(wit_types::OpResult, wit_types::Effects)> {
        match self.state {
            TestOpState::Returned { result, effects } => Ok((*result, *effects)),
            TestOpState::Suspended(_) => Err(Error::ProviderProtocol(
                "operation is still suspended".to_string(),
            )),
        }
    }
}

impl std::fmt::Debug for TestOp<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("TestOp");
        debug.field("id", &self.id).field("op", &self.op);
        match &self.state {
            TestOpState::Suspended(callouts) => {
                debug.field("state", &"suspended");
                debug.field("callouts", callouts);
            },
            TestOpState::Returned { result, effects } => {
                debug.field("state", &"returned");
                debug.field("result", result);
                debug.field("effects", effects);
            },
        }
        debug.finish()
    }
}

fn finish_initialize_return(
    ret: wit_types::ProviderReturn,
) -> std::result::Result<wit_types::InitializeResult, BuildError> {
    op_validate::validate_return(&Op::Initialize, &ret, |_| false).map_err(|message| {
        BuildError::ProviderProtocol(format!("initialize returned invalid result: {message}"))
    })?;
    match ret.result {
        wit_types::OpResult::Initialize(result) => Ok(result),
        other => Err(BuildError::ProviderProtocol(format!(
            "initialize returned unexpected result: {other:?}"
        ))),
    }
}

fn validate_instance_config(
    schema: Option<&serde_json::Value>,
    config: &Resolved,
    mount_name: &str,
) -> std::result::Result<(), BuildError> {
    let Some(schema) = schema else {
        return Ok(());
    };

    let empty_config = serde_json::Value::Object(serde_json::Map::new());
    let config_value = config
        .spec
        .config_raw
        .as_ref()
        .map_or(&empty_config, ProviderConfig::as_value);
    match omnifs_provider::validate_config(schema, config_value) {
        Ok(()) => Ok(()),
        Err(omnifs_provider::SchemaError::Validation(error)) => Err(BuildError::InvalidConfig(
            format!("config for mount {mount_name} failed validation: {error}"),
        )),
        Err(omnifs_provider::SchemaError::InvalidSchema(error)) => {
            Err(BuildError::ProviderProtocol(format!(
                "provider config schema for mount {mount_name} is invalid: {error}"
            )))
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{BuildError, CacheDirs};

    #[test]
    fn provider_cache_dirs_are_created_before_runtime_uses_them() {
        let dir = tempfile::tempdir().unwrap();

        let cache_dirs = CacheDirs::prepare(dir.path(), "linear").unwrap();

        assert!(cache_dirs.blob.is_dir());
        assert!(cache_dirs.archive_root.is_dir());
    }

    #[test]
    fn provider_cache_dir_creation_failure_stops_runtime_build() {
        let dir = tempfile::tempdir().unwrap();
        let blob_path = CacheDirs::blob_path(dir.path(), "linear");
        std::fs::create_dir_all(blob_path.parent().unwrap()).unwrap();
        std::fs::write(&blob_path, "not a directory").unwrap();

        let error = CacheDirs::prepare(dir.path(), "linear").unwrap_err();

        assert!(matches!(
            error,
            BuildError::CacheDir { path, .. } if path == blob_path
        ));
    }
}

/// Test-only re-exports used by the `callout_tracing` integration test
/// to drive canned futures/results through the same instrumentation
/// pattern as `dispatch_one` without spinning up real executors.
#[doc(hidden)]
pub mod __test_support {
    use crate::callouts::{CalloutKind, record_outcome as inner_record};
    use crate::log_redaction::{LogUrl as InternalLogUrl, WitHeaders as InternalWitHeaders};
    use omnifs_wit::provider::types as wit_types;
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
}
