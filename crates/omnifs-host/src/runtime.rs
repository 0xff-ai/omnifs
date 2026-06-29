//! Engine/instance/mount lifecycle for one WASM provider.
//!
//! `Runtime` manages the Wasmtime store lifetime, provider initialization,
//! executor handles (HTTP, Git, Blob, Archive), and cache/mount lifecycle.
//! Op execution is in `op_lifecycle`; WASI store plumbing is in `wasi`.

use crate::archive::ArchiveExecutor;
use crate::auth::{AuthManager, credential_store_for_file};
use crate::blob::{BlobExecutor, BlobLimits};
use crate::blob_cache::BlobCache;
use crate::callouts::{CalloutHost, TestCallout, TestCallouts};
use crate::capability::{CapabilityChecker, config_str};
use crate::cloner::GitCloner;
use crate::git;
use crate::http::HttpStack;
use crate::inflight::InFlight;
use crate::inspector::{self, InspectorSink};
use crate::instance::Instance;
use crate::invalidation::InvalidationState;
use crate::manifest::Artifact;
use crate::tree_refs::TreeRefs;
use dashmap::DashMap;
use omnifs_cache::{Caches, Store};
use omnifs_caps::{Grant, PreopenedPath};
use omnifs_core::ProviderId;
use omnifs_core::path::Path;
use omnifs_mount::mounts::Spec;
use omnifs_provider::{ConfigMetadata, HostResourceBinding, ProviderStore};
use omnifs_wit::provider::types as wit_types;

use std::io;
use std::path::{Path as StdPath, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;

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

    /// `<hex>.wasm` for a pinned provider id: the serving path the
    /// daemon loads. Identity is the content hash, never a filename.
    pub(crate) fn provider_path_by_id(&self, id: &ProviderId) -> PathBuf {
        ProviderStore::new(&self.providers_dir).artifact_path(id)
    }
}

/// Runtime for one mounted WASM provider component.
///
/// Manages the Wasmtime instance driver, host callout imports, cache state,
/// and operation id allocation.
pub struct Runtime {
    pub(crate) instance: Instance,
    initialize_result: wit_types::InitializeResult,
    pub(crate) mount_name: String,
    pub(crate) provider_name: String,
    next_operation_id: AtomicU64,
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
    test_callouts: Option<std::sync::Mutex<mpsc::Receiver<TestCallout>>>,
}

/// Test operation driver used by provider integration tests that need to
/// inspect and answer captured host imports. This is not the provider runtime
/// protocol: production operations await WIT async host imports directly.
#[doc(hidden)]
pub struct TestOp<'a> {
    runtime: &'a Runtime,
    op: Op,
    id: u64,
    op_gen: u64,
    state: TestOpState,
}

enum TestOpState {
    InProgress,
    WaitingForCallouts {
        callouts: Vec<wit_types::Callout>,
        replies: Vec<tokio::sync::oneshot::Sender<wit_types::CalloutResult>>,
        result_rx: mpsc::Receiver<std::result::Result<wit_types::ProviderReturn, Error>>,
    },
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
        config: &Spec,
        cloner: Arc<GitCloner>,
        context: &HostContext,
        caches: &Arc<Caches>,
    ) -> std::result::Result<Self, BuildError> {
        Self::build(engine, wasm_path, config, cloner, context, caches, false)
    }

    #[doc(hidden)]
    pub fn new_for_callout_tests(
        engine: &wasmtime::Engine,
        wasm_path: &StdPath,
        config: &Spec,
        cloner: Arc<GitCloner>,
        context: &HostContext,
        caches: &Arc<Caches>,
    ) -> std::result::Result<Self, BuildError> {
        Self::build(engine, wasm_path, config, cloner, context, caches, true)
    }

    fn build(
        engine: &wasmtime::Engine,
        wasm_path: &StdPath,
        config: &Spec,
        cloner: Arc<GitCloner>,
        context: &HostContext,
        caches: &Arc<Caches>,
        capture_test_callouts: bool,
    ) -> std::result::Result<Self, BuildError> {
        let (test_callouts, test_rx) = if capture_test_callouts {
            let (test_callouts, rx) = TestCallouts::channel();
            (Some(test_callouts), Some(rx))
        } else {
            (None, None)
        };
        let mount_name = config.mount.as_str();
        let config_bytes = config.config_bytes();
        // Load the pinned artifact's manifest once: capability/config metadata
        // validation, preopen resolution, and auth all read it, and enforcement
        // rests on this pinned manifest, never on a spec-stamped snapshot.
        let manifest = Artifact::load(wasm_path)
            .and_then(|artifact| artifact.metadata())
            .map_err(BuildError::InvalidConfig)?;
        let config_metadata = manifest
            .as_ref()
            .and_then(|manifest| manifest.config.as_ref())
            .cloned();

        let preopens = resolve_preopens(config, config_metadata.as_ref());
        let instance = Instance::new(engine, wasm_path, config_bytes, &preopens)?;

        validate_instance_config(config_metadata.as_ref(), config, mount_name)?;

        let init_return = instance.initialize().map_err(BuildError::from)?;
        let initialize_result = finish_initialize_return(init_return)?;
        let capability = Arc::new(CapabilityChecker::from_config(
            config,
            &initialize_result.capabilities,
            config_metadata.as_ref(),
        ));

        let auth_manifest = manifest
            .as_ref()
            .and_then(omnifs_provider::ProviderManifest::wasm_auth_manifest);
        let auth = if config.auth.is_none() {
            Arc::new(AuthManager::none())
        } else {
            let store = credential_store_for_file(context.credentials_file());
            Arc::new(
                AuthManager::from_configs_manifest_store_with_store(
                    config.auth.as_slice(),
                    auth_manifest.as_ref(),
                    config.provider_name().as_str(),
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
        ));

        // Per-mount facade: scopes all cache keys with "{mount}\x1f".
        let cache = caches.mount(mount_name);
        let blob_limits = BlobLimits::from_config(config);
        let http = Arc::new(HttpStack::new(auth.clone(), capability.clone())?);
        let blob = BlobExecutor::new(Arc::clone(&http), blob_cache.clone(), blob_limits);
        let mut callout_host = CalloutHost::new(
            Arc::clone(&http),
            git.clone(),
            blob.clone(),
            Arc::clone(&archive),
        );
        if let Some(test_callouts) = test_callouts {
            callout_host = callout_host.with_test_callouts(test_callouts);
        }
        instance
            .set_callouts(callout_host)
            .map_err(BuildError::from)?;
        Ok(Self {
            instance,
            initialize_result,
            mount_name: mount_name.to_string(),
            provider_name: config.provider_name().to_string(),
            next_operation_id: AtomicU64::new(1),
            blob_cache,
            trees,
            cache,
            invalidation: InvalidationState::default(),
            inflight: InFlight::new(),
            pagination_locks: DashMap::new(),
            rate_limit_until: std::sync::Mutex::new(None),
            inspector: inspector::global(),
            test_callouts: test_rx.map(std::sync::Mutex::new),
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

    /// Non-blocking receive of the next captured provider callout, if one has
    /// been issued and not yet answered. Only yields values on runtimes built
    /// with [`Runtime::new_for_callout_tests`]; returns `None` otherwise or when
    /// no callout is pending. Lets a concurrency test observe that two ops are
    /// suspended on host imports at the same instant before answering either.
    #[doc(hidden)]
    pub fn try_recv_test_callout(&self) -> Option<PendingTestCallout> {
        let received = self.test_callouts.as_ref()?.lock().ok()?.try_recv().ok()?;
        Some(PendingTestCallout {
            op_id: received.op_id,
            reply: received.reply,
        })
    }
}

/// Test-only handle to one captured provider callout awaiting its answer. See
/// [`Runtime::try_recv_test_callout`].
#[doc(hidden)]
pub struct PendingTestCallout {
    op_id: u64,
    reply: tokio::sync::oneshot::Sender<wit_types::CalloutResult>,
}

impl PendingTestCallout {
    #[doc(hidden)]
    #[must_use]
    pub fn op_id(&self) -> u64 {
        self.op_id
    }

    /// Resume the suspended provider future with `result`.
    #[doc(hidden)]
    pub fn answer(self, result: wit_types::CalloutResult) {
        let _ = self.reply.send(result);
    }
}

impl Runtime {
    /// Synchronous test entry: blocks the caller until the operation returns or
    /// suspends on captured callouts. Production code drives ops through the
    /// async [`Runtime::run_op`] path instead; this exists for the provider
    /// integration harness (`omnifs-itest`).
    #[doc(hidden)]
    pub fn start_op(&self, op: Op) -> Result<TestOp<'_>> {
        let op_gen = self.cache.current_generation();
        let id = self.next_operation_id();
        if self.test_callouts.is_some() {
            return TestOp::start_callout_test(self, op, id, op_gen);
        }
        let ret = futures::executor::block_on(self.instance.start_op(op.clone(), id))?;
        TestOp::from_return(self, op, id, op_gen, ret)
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
    fn start_callout_test(runtime: &'a Runtime, op: Op, id: u64, op_gen: u64) -> Result<Self> {
        let instance = runtime.instance.clone();
        let op_for_task = op.clone();
        let (result_tx, result_rx) = mpsc::channel();
        std::thread::Builder::new()
            .name(format!("omnifs-test-op-{id}"))
            .spawn(move || {
                let result = futures::executor::block_on(instance.start_op(op_for_task, id));
                let _ = result_tx.send(result);
            })
            .map_err(|error| Error::ProviderProtocol(format!("spawn test op: {error}")))?;

        let state = Self::wait_for_progress(runtime, &op, id, op_gen, result_rx)?;
        Ok(Self {
            runtime,
            op,
            id,
            op_gen,
            state,
        })
    }

    fn from_return(
        runtime: &'a Runtime,
        op: Op,
        id: u64,
        op_gen: u64,
        ret: wit_types::ProviderReturn,
    ) -> Result<Self> {
        let state = Self::returned_state(runtime, &op, op_gen, ret)?;
        Ok(Self {
            runtime,
            op,
            id,
            op_gen,
            state,
        })
    }

    fn wait_for_progress(
        runtime: &Runtime,
        op: &Op,
        id: u64,
        op_gen: u64,
        result_rx: mpsc::Receiver<std::result::Result<wit_types::ProviderReturn, Error>>,
    ) -> Result<TestOpState> {
        let inbox = runtime.test_callouts.as_ref().ok_or_else(|| {
            Error::ProviderProtocol("test callout inbox is not configured".to_string())
        })?;
        let recv_callout = |timeout| {
            inbox
                .lock()
                .expect("test callout receiver poisoned")
                .recv_timeout(timeout)
        };
        loop {
            match result_rx.try_recv() {
                Ok(ret) => return Self::returned_state(runtime, op, op_gen, ret?),
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(Error::ProviderProtocol(
                        "provider operation result channel closed".to_string(),
                    ));
                },
                Err(mpsc::TryRecvError::Empty) => {},
            }

            match recv_callout(std::time::Duration::from_millis(10)) {
                Ok(first) => {
                    let mut callouts = Vec::new();
                    let mut replies = Vec::new();
                    Self::push_test_callout(id, first, &mut callouts, &mut replies)?;
                    while let Ok(next) = recv_callout(std::time::Duration::from_millis(1)) {
                        Self::push_test_callout(id, next, &mut callouts, &mut replies)?;
                    }
                    return Ok(TestOpState::WaitingForCallouts {
                        callouts,
                        replies,
                        result_rx,
                    });
                },
                Err(mpsc::RecvTimeoutError::Timeout) => {},
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(Error::ProviderProtocol(
                        "test callout receiver closed".to_string(),
                    ));
                },
            }
        }
    }

    fn push_test_callout(
        id: u64,
        test_callout: TestCallout,
        callouts: &mut Vec<wit_types::Callout>,
        replies: &mut Vec<tokio::sync::oneshot::Sender<wit_types::CalloutResult>>,
    ) -> Result<()> {
        if test_callout.op_id != id {
            return Err(Error::ProviderProtocol(format!(
                "test callout for operation {} received while driving operation {id}",
                test_callout.op_id
            )));
        }
        callouts.push(test_callout.callout);
        replies.push(test_callout.reply);
        Ok(())
    }

    fn returned_state(
        runtime: &Runtime,
        op: &Op,
        op_gen: u64,
        ret: wit_types::ProviderReturn,
    ) -> Result<TestOpState> {
        let effects = ret.effects.clone();
        let result = runtime.finish_provider_return(op, ret, op_gen)?;
        runtime.note_returned_result(&result);
        Ok(TestOpState::Returned {
            result: Box::new(result),
            effects: Box::new(effects),
        })
    }

    pub fn callouts(&self) -> &[wit_types::Callout] {
        match &self.state {
            TestOpState::WaitingForCallouts { callouts, .. } => callouts,
            TestOpState::InProgress | TestOpState::Returned { .. } => &[],
        }
    }

    pub fn is_waiting_for_callouts(&self) -> bool {
        matches!(self.state, TestOpState::WaitingForCallouts { .. })
    }

    pub fn is_returned(&self) -> bool {
        matches!(self.state, TestOpState::Returned { .. })
    }

    #[allow(clippy::needless_pass_by_value)]
    pub fn answer_callouts(&mut self, results: Vec<wit_types::CalloutResult>) -> Result<()> {
        let state = std::mem::replace(&mut self.state, TestOpState::InProgress);
        let TestOpState::WaitingForCallouts {
            replies, result_rx, ..
        } = state
        else {
            return Err(Error::ProviderProtocol(
                "provider operation is not waiting on test callouts".to_string(),
            ));
        };
        if results.len() != replies.len() {
            return Err(Error::ProviderProtocol(format!(
                "expected {} test callout results, got {}",
                replies.len(),
                results.len()
            )));
        }
        for (reply, result) in replies.into_iter().zip(results) {
            let _ = reply.send(result);
        }
        self.state =
            Self::wait_for_progress(self.runtime, &self.op, self.id, self.op_gen, result_rx)?;
        Ok(())
    }

    pub fn into_result(self) -> Result<wit_types::OpResult> {
        match self.state {
            TestOpState::Returned { result, .. } => Ok(*result),
            TestOpState::WaitingForCallouts { .. } | TestOpState::InProgress => Err(
                Error::ProviderProtocol("provider operation has not returned".to_string()),
            ),
        }
    }

    pub fn result(&self) -> Option<&wit_types::OpResult> {
        match &self.state {
            TestOpState::Returned { result, .. } => Some(result.as_ref()),
            TestOpState::WaitingForCallouts { .. } | TestOpState::InProgress => None,
        }
    }

    pub fn effects(&self) -> Option<&wit_types::Effects> {
        match &self.state {
            TestOpState::Returned { effects, .. } => Some(effects.as_ref()),
            TestOpState::WaitingForCallouts { .. } | TestOpState::InProgress => None,
        }
    }

    pub fn into_result_and_effects(self) -> Result<(wit_types::OpResult, wit_types::Effects)> {
        match self.state {
            TestOpState::Returned { result, effects } => Ok((*result, *effects)),
            TestOpState::WaitingForCallouts { .. } | TestOpState::InProgress => Err(
                Error::ProviderProtocol("provider operation has not returned".to_string()),
            ),
        }
    }
}

impl std::fmt::Debug for TestOp<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("TestOp");
        debug.field("id", &self.id).field("op", &self.op);
        match &self.state {
            TestOpState::InProgress => {
                debug.field("state", &"in_progress");
            },
            TestOpState::WaitingForCallouts { callouts, .. } => {
                debug.field("state", &"waiting-for-callouts");
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

/// The WASI preopens to hand the instance. A literal preopen grant is used
/// verbatim; a dynamic one is resolved at mount-start from the config fields the
/// provider marks as host files: the file's parent directory is preopened at the
/// same path (guest == host), so the provider opens the configured path
/// unchanged. The mode comes from the field's host-resource binding.
fn resolve_preopens(config: &Spec, metadata: Option<&ConfigMetadata>) -> Vec<PreopenedPath> {
    match config
        .capabilities
        .as_ref()
        .and_then(|capabilities| capabilities.preopened_paths.as_ref())
    {
        Some(Grant::Literal(paths)) => paths.clone(),
        Some(Grant::Dynamic(_)) => metadata
            .into_iter()
            .flat_map(ConfigMetadata::host_resource_fields)
            .filter_map(|(field, metadata)| {
                let Some(HostResourceBinding::File { mode }) = metadata.binding else {
                    return None;
                };
                let value = config_str(config, field)?;
                let dir = StdPath::new(value).parent()?.to_str()?.to_string();
                Some(PreopenedPath {
                    host: dir.clone(),
                    guest: dir,
                    mode,
                })
            })
            .collect(),
        None => Vec::new(),
    }
}

fn validate_instance_config(
    metadata: Option<&ConfigMetadata>,
    config: &Spec,
    mount_name: &str,
) -> std::result::Result<(), BuildError> {
    let Some(metadata) = metadata else {
        return Ok(());
    };

    let empty_config = serde_json::Value::Object(serde_json::Map::new());
    let config_value = config.config_raw.as_ref().unwrap_or(&empty_config);
    match metadata.validate_config(config_value) {
        Ok(()) => Ok(()),
        Err(error) => Err(BuildError::InvalidConfig(format!(
            "config for mount {mount_name} failed validation: {error}"
        ))),
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
    /// with the internal `CalloutKind` strum labels.
    pub fn kind_label(callout: &wit_types::Callout) -> &'static str {
        CalloutKind::of(callout).into()
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
