//! Engine/instance/mount lifecycle for one WASM provider.
//!
//! `Runtime` manages the Wasmtime store lifetime, provider initialization,
//! executor handles (HTTP, Git, and Blob), and cache/mount lifecycle.
//! Typed operation execution is in `ops::lifecycle`; WASI store plumbing is in `wasi`.

use crate::auth::binding_from_config;
use crate::authority::RuntimeAuthority;
use crate::blob::{BlobExecutor, BlobLimits};
use crate::blob_cache::BlobCache;
use crate::cache::{Caches, Store};
use crate::callouts::{CalloutHost, TestCallouts, TestSignal};
use crate::cloner::GitCloner;
use crate::coalesce::ns::InFlight;
use crate::git;
use crate::http::HttpStack;
use crate::instance::Instance;
use crate::invalidation::InvalidationState;
use crate::tree_refs::TreeRefs;
use dashmap::DashMap;
use omnifs_auth::{AuthBinding, CredentialHealth, CredentialService};
use omnifs_core::path::Path;
use omnifs_wit::provider::types as wit_types;
use omnifs_workspace::ids::ProviderId;
use omnifs_workspace::mounts::Spec;
use omnifs_workspace::provider::{ConfigMetadata, ProviderAuthManifest, ProviderStore};

use std::fs;
use std::path::{Path as StdPath, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use tracing::debug;

pub(crate) mod instance;
pub(crate) mod registry;
pub(crate) mod wasi;
pub(crate) mod wasm;

use crate::clock::{self, DYNAMIC_TTL_MILLIS};
use crate::object_id::ObjectId;
use crate::op_validate;

pub(crate) const HTTP_FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
// Host-side 429 cooldown when the provider error carried no Retry-After.
const RATE_LIMIT_DEFAULT_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(5);
// Upper bound so a hostile Retry-After cannot overflow `Instant` or wedge the
// window open indefinitely.
const RATE_LIMIT_MAX_COOLDOWN: std::time::Duration = std::time::Duration::from_hours(1);
const PROVIDER_CACHE_SUBDIR: &str = "providers";
const BLOB_CACHE_SUBDIR: &str = "blobs";
const RECENT_REVALIDATE_OBJECTS: usize = 32;

/// Host-owned filesystem context for provider runtime.
#[derive(Clone, Debug)]
pub struct HostContext {
    cache_dir: PathBuf,
    wasm_cache_dir: PathBuf,
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
        let cache_dir = cache_dir.as_ref().to_path_buf();
        Self {
            wasm_cache_dir: omnifs_workspace::layout::wasm_cache_dir(&cache_dir),
            cache_dir,
            config_dir: config_dir.as_ref().to_path_buf(),
            providers_dir: providers_dir.as_ref().to_path_buf(),
            credentials_file: credentials_file.as_ref().to_path_buf(),
        }
    }

    /// Store compiled provider components separately from runtime cache data.
    ///
    /// Production hosts use the default path under [`Self::cache_dir`]. Test
    /// harnesses can share compiled artifacts across otherwise-hermetic cache
    /// directories so each test process does not compile the same component.
    #[must_use]
    pub fn with_wasm_cache_dir(mut self, wasm_cache_dir: impl AsRef<StdPath>) -> Self {
        self.wasm_cache_dir = wasm_cache_dir.as_ref().to_path_buf();
        self
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

    pub(crate) fn wasm_cache_dir(&self) -> &StdPath {
        &self.wasm_cache_dir
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
    provider_id: ProviderId,
    auth: Option<Arc<AuthBinding>>,
    next_operation_id: AtomicU64,
    blob_cache: Arc<BlobCache>,
    trees: Arc<TreeRefs>,
    pub(crate) cache: Store,
    pub(crate) invalidation: InvalidationState,
    pub(crate) coalesce: InFlight,
    recent_objects: parking_lot::Mutex<RecentObjects>,
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
    pub(crate) test_callouts: Option<std::sync::Mutex<mpsc::Receiver<TestSignal>>>,
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
    #[error("authority: {0}")]
    Authority(#[from] crate::authority::AuthorityError),
    #[error("cache: {0}")]
    Cache(String),
    #[error("provider protocol: {0}")]
    ProviderProtocol(String),
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("wasmtime: {0}")]
    Wasmtime(#[from] wasmtime::Error),
    #[error("provider protocol: {0}")]
    ProviderProtocol(String),
    #[error("provider returned error: {0:?}")]
    ProviderError(wit_types::ProviderError),
}

pub(crate) type Result<T> = std::result::Result<T, EngineError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderErrorClass {
    NotFound,
    NotDirectory,
    IsDirectory,
    PermissionDenied,
    InvalidInput,
    TooLarge,
    RateLimited,
    Timeout,
    Network,
    Internal,
}

impl EngineError {
    pub(crate) fn provider_class(&self) -> Option<ProviderErrorClass> {
        let Self::ProviderError(error) = self else {
            return None;
        };
        Some(error.kind.into())
    }

    pub(crate) fn is_provider_rate_limited(&self) -> bool {
        self.provider_class() == Some(ProviderErrorClass::RateLimited)
    }

    pub(crate) fn is_provider_not_found_or_invalid_input(&self) -> bool {
        matches!(
            self.provider_class(),
            Some(ProviderErrorClass::NotFound | ProviderErrorClass::InvalidInput)
        )
    }
}

impl From<wit_types::ErrorKind> for ProviderErrorClass {
    fn from(kind: wit_types::ErrorKind) -> Self {
        match kind {
            wit_types::ErrorKind::NotFound => Self::NotFound,
            wit_types::ErrorKind::NotADirectory => Self::NotDirectory,
            wit_types::ErrorKind::NotAFile => Self::IsDirectory,
            wit_types::ErrorKind::PermissionDenied | wit_types::ErrorKind::Denied => {
                Self::PermissionDenied
            },
            wit_types::ErrorKind::InvalidInput => Self::InvalidInput,
            wit_types::ErrorKind::TooLarge => Self::TooLarge,
            wit_types::ErrorKind::RateLimited => Self::RateLimited,
            wit_types::ErrorKind::Network => Self::Network,
            wit_types::ErrorKind::Timeout => Self::Timeout,
            wit_types::ErrorKind::VersionMismatch | wit_types::ErrorKind::Internal => {
                Self::Internal
            },
        }
    }
}

impl From<EngineError> for BuildError {
    fn from(err: EngineError) -> Self {
        match err {
            EngineError::Wasmtime(e) => Self::Wasmtime(e),
            EngineError::ProviderProtocol(msg) => Self::ProviderProtocol(msg),
            EngineError::ProviderError(e) => {
                Self::ProviderProtocol(format!("provider error during build: {e:?}"))
            },
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

    pub fn provider_id(&self) -> ProviderId {
        self.provider_id
    }

    pub fn auth_health(&self) -> Option<CredentialHealth> {
        self.auth.as_ref().map(|binding| binding.health())
    }

    pub(crate) fn auth_binding(&self) -> Option<&Arc<AuthBinding>> {
        self.auth.as_ref()
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
        credential_service: &Arc<CredentialService>,
    ) -> std::result::Result<Self, BuildError> {
        Self::build(
            engine,
            wasm_path,
            config,
            cloner,
            context,
            caches,
            credential_service,
            false,
        )
    }

    #[doc(hidden)]
    pub fn new_for_callout_tests(
        engine: &wasmtime::Engine,
        wasm_path: &StdPath,
        config: &Spec,
        cloner: Arc<GitCloner>,
        context: &HostContext,
        caches: &Arc<Caches>,
        credential_service: &Arc<CredentialService>,
    ) -> std::result::Result<Self, BuildError> {
        Self::build(
            engine,
            wasm_path,
            config,
            cloner,
            context,
            caches,
            credential_service,
            true,
        )
    }

    #[allow(clippy::too_many_arguments)]
    // Keep mount construction ordered in one boundary: manifest validation,
    // provider initialization, auth, caches, and runtime wiring are coupled.
    #[allow(clippy::too_many_lines)]
    fn build(
        engine: &wasmtime::Engine,
        wasm_path: &StdPath,
        config: &Spec,
        cloner: Arc<GitCloner>,
        context: &HostContext,
        caches: &Arc<Caches>,
        credential_service: &Arc<CredentialService>,
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
        // Load the pinned artifact's manifest once: config validation must run
        // before preopen resolution or instance creation, and capability/auth
        // enforcement rests on this pinned manifest, never on a spec-stamped
        // snapshot.
        let manifest = fs::read(wasm_path)
            .map_err(|error| format!("reading {}: {error}", wasm_path.display()))
            .and_then(|bytes| {
                omnifs_workspace::provider::ProviderWasm::from_bytes(bytes)
                    .metadata()
                    .map_err(|error| error.to_string())
            })
            .map_err(BuildError::InvalidConfig)?;
        let manifest = manifest.ok_or_else(|| {
            BuildError::InvalidConfig("provider artifact has no embedded manifest".to_owned())
        })?;
        let config_metadata = manifest.config.as_ref();

        validate_instance_config(config_metadata, config, mount_name)?;

        let authority = RuntimeAuthority::resolve(&manifest, config)?;
        let park_signal = test_callouts.as_ref().map(TestCallouts::park_signal);
        let instance = Instance::new(
            engine,
            wasm_path,
            config_bytes,
            Arc::clone(&authority),
            park_signal,
        )?;

        let init_return = instance.initialize().map_err(BuildError::from)?;
        let (initialize_result, initialize_effects) = finish_initialize_return(init_return)?;
        let auth_manifest = manifest
            .auth
            .as_ref()
            .map(ProviderAuthManifest::wasm_auth_manifest);
        let auth = binding_from_config(
            config.auth.as_ref(),
            auth_manifest.as_ref(),
            config.provider_name().as_str(),
            credential_service,
        )
        .map_err(|e| BuildError::ProviderProtocol(format!("auth config error: {e}")))?;

        let trees = Arc::new(TreeRefs::new());
        let git = git::GitExecutor::new(cloner, Arc::clone(&authority), trees.clone(), mount_name);

        let cache_root = context
            .cache_dir()
            .join(PROVIDER_CACHE_SUBDIR)
            .join(mount_name);
        let blob_path = cache_root.join(BLOB_CACHE_SUBDIR);
        let blob_cache = Arc::new(BlobCache::new(blob_path.clone()).map_err(|source| {
            BuildError::Cache(format!("blob cache at {}: {source}", blob_path.display()))
        })?);
        // Per-mount facade: structurally isolates object and view cache state.
        let cache = caches.mount(mount_name);
        let blob_limits = BlobLimits::from_config(config);
        let http = Arc::new(HttpStack::new(auth.clone(), authority)?);
        let blob = BlobExecutor::new(Arc::clone(&http), blob_cache.clone(), blob_limits);
        let mut callout_host = CalloutHost::new(Arc::clone(&http), git.clone(), blob.clone());
        if let Some(test_callouts) = test_callouts {
            callout_host = callout_host.with_test_callouts(test_callouts);
        }
        instance
            .set_callouts(callout_host)
            .map_err(BuildError::from)?;
        let runtime = Self {
            instance,
            initialize_result,
            mount_name: mount_name.to_string(),
            provider_name: config.provider_name().to_string(),
            provider_id: config.provider.id,
            auth,
            next_operation_id: AtomicU64::new(1),
            blob_cache,
            trees,
            cache,
            invalidation: InvalidationState::default(),
            coalesce: InFlight::new(),
            recent_objects: parking_lot::Mutex::new(RecentObjects::new()),
            pagination_locks: DashMap::new(),
            rate_limit_until: std::sync::Mutex::new(None),
            test_callouts: test_rx.map(std::sync::Mutex::new),
        };
        runtime.publish_effects(&initialize_effects, runtime.cache.current_generation());
        Ok(runtime)
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
            crate::effect_apply::EffectApplier::new(&self.cache).apply(effects, op_gen, now);
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

    pub(crate) fn note_read_object(&self, id: ObjectId) {
        self.recent_objects.lock().record(id);
    }

    pub(crate) async fn revalidate_recent_objects(&self) {
        let ids = self.recent_objects.lock().snapshot();
        for id in ids {
            let Some(path) = self.revalidation_path_for(&id) else {
                continue;
            };
            let content_type = path.content_type_mime(None).to_string();
            if let Err(error) = self.namespace().revalidate_file(&path, content_type).await {
                debug!(
                    mount = self.mount_name.as_str(),
                    path = path.as_str(),
                    error = %error,
                    "host revalidation read failed"
                );
            }
        }
    }

    fn revalidation_path_for(&self, id: &ObjectId) -> Option<Path> {
        self.cache.paths_for_id(id.as_bytes()).into_iter().next()
    }

    pub async fn call_timer_tick(&self) -> Result<()> {
        self.run_event(wit_types::ProviderEvent::TimerTick).await
    }

    /// Resolve a tree-ref handle to a real filesystem path.
    /// Resolves a host-issued git tree handle through the shared registry.
    pub fn resolve_tree_ref(&self, tree_ref: u64) -> Option<PathBuf> {
        self.trees.resolve(tree_ref)
    }

    /// Serve the canonical bytes for `path` from the anchor-keyed object
    /// cache. When a `read-file` terminal answers `byte-source::canonical`, the
    /// tree resolves the longest covering anchor and returns those bytes
    /// without copying across the WIT. `None` when no stored anchor covers
    /// `path`.
    pub fn canonical_bytes_for(&self, path: &Path) -> Option<Vec<u8>> {
        self.cache
            .cached_canonical_for(path)
            .map(|canonical| canonical.bytes)
    }

    /// Read the full bytes of a stored blob for a blob-backed `read-file`
    /// terminal.
    pub fn read_blob_full(&self, blob_id: u64) -> Result<Vec<u8>> {
        let record = self
            .blob_cache
            .lookup_by_id(blob_id)
            .ok_or_else(|| EngineError::ProviderProtocol(format!("blob {blob_id} not found")))?;
        let path = self.blob_cache.body_path(&record);
        std::fs::read(path)
            .map_err(|e| EngineError::ProviderProtocol(format!("read blob {blob_id}: {e}")))
    }

    pub(crate) fn publish_effects(&self, effects: &wit_types::Effects, op_gen: u64) {
        let now = clock::now_millis();
        let (prefixes, paths) =
            crate::effect_apply::EffectApplier::new(&self.cache).apply(effects, op_gen, now);
        self.record_view_invalidations(prefixes, paths);
    }
}

struct RecentObjects {
    ids: Vec<ObjectId>,
}

impl RecentObjects {
    fn new() -> Self {
        Self {
            ids: Vec::with_capacity(RECENT_REVALIDATE_OBJECTS),
        }
    }

    fn record(&mut self, id: ObjectId) {
        if let Some(index) = self.ids.iter().position(|existing| existing == &id) {
            self.ids.remove(index);
        }
        self.ids.insert(0, id);
        self.ids.truncate(RECENT_REVALIDATE_OBJECTS);
    }

    fn snapshot(&self) -> Vec<ObjectId> {
        self.ids.clone()
    }
}

fn finish_initialize_return(
    ret: (
        std::result::Result<wit_types::InitializeResult, wit_types::ProviderError>,
        wit_types::Effects,
    ),
) -> std::result::Result<(wit_types::InitializeResult, wit_types::Effects), BuildError> {
    let (result, effects) = ret;
    op_validate::validate_initialize(&result, &effects, |_| false).map_err(|message| {
        BuildError::ProviderProtocol(format!("initialize returned invalid result: {message}"))
    })?;
    result
        .map(|result| (result, effects))
        .map_err(EngineError::ProviderError)
        .map_err(BuildError::from)
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
