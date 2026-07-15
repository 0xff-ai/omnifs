//! Engine/instance/mount lifecycle for one WASM provider.
//!
//! `Runtime` manages the Wasmtime store lifetime, provider initialization,
//! executor handles (HTTP, Git, and Blob), and cache/mount lifecycle.
//! Typed operation execution is in `ops::lifecycle`; WASI store plumbing is in `wasi`.

use crate::auth::binding_from_config;
use crate::authority::RuntimeAuthority;
use crate::blob::{BlobExecutor, BlobLimits};
use crate::cache::{Caches, MountResources};
use crate::callouts::{CalloutHost, TestCallouts, TestSignal};
use crate::cloner::GitCloner;
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
use omnifs_workspace::provider::{
    ConfigMetadata, ProviderAuthManifest, ProviderManifest, ProviderStore,
};

use std::path::{Path as StdPath, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;

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
    pub(crate) mount_name: String,
    pub(crate) provider_name: String,
    provider_id: ProviderId,
    auth: Option<Arc<AuthBinding>>,
    next_operation_id: AtomicU64,
    pub resources: Arc<MountResources>,
    trees: Arc<TreeRefs>,
    pub(crate) invalidation: InvalidationState,
    pub(crate) namespace_flights: crate::ops::namespace::NamespaceFlights,
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

    pub fn new(
        engine: &wasmtime::Engine,
        wasm_path: &StdPath,
        config: &Spec,
        manifest: &ProviderManifest,
        cloner: Arc<GitCloner>,
        context: &HostContext,
        caches: &Arc<Caches>,
        credential_service: &Arc<CredentialService>,
    ) -> std::result::Result<Self, BuildError> {
        Self::build(
            engine,
            wasm_path,
            config,
            manifest,
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
        manifest: &ProviderManifest,
        cloner: Arc<GitCloner>,
        context: &HostContext,
        caches: &Arc<Caches>,
        credential_service: &Arc<CredentialService>,
    ) -> std::result::Result<Self, BuildError> {
        Self::build(
            engine,
            wasm_path,
            config,
            manifest,
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
        manifest: &ProviderManifest,
        cloner: Arc<GitCloner>,
        _context: &HostContext,
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

        let (init_result, initialize_effects) = instance.initialize().map_err(BuildError::from)?;
        op_validate::validate_initialize(&init_result, &initialize_effects, |_| false).map_err(
            |message| {
                BuildError::ProviderProtocol(format!(
                    "initialize returned invalid result: {message}"
                ))
            },
        )?;
        let initialize_effects = init_result
            .map(|_| initialize_effects)
            .map_err(EngineError::ProviderError)
            .map_err(BuildError::from)?;
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

        let name = omnifs_workspace::mounts::Name::new(mount_name)
            .map_err(|error| BuildError::Cache(error.to_string()))?;
        let resources = caches
            .mount(&name)
            .map_err(|error| BuildError::Cache(error.to_string()))?;
        let blob_limits = BlobLimits::from_config(config);
        let http = Arc::new(HttpStack::new(auth.clone(), authority)?);
        let blob = BlobExecutor::new(Arc::clone(&http), Arc::clone(&resources), blob_limits);
        let mut callout_host = CalloutHost::new(Arc::clone(&http), git.clone(), blob.clone());
        if let Some(test_callouts) = test_callouts {
            callout_host = callout_host.with_test_callouts(test_callouts);
        }
        instance
            .set_callouts(callout_host)
            .map_err(BuildError::from)?;
        let runtime = Self {
            instance,
            mount_name: mount_name.to_string(),
            provider_name: config.provider_name().to_string(),
            provider_id: config.provider.id,
            auth,
            next_operation_id: AtomicU64::new(1),
            resources,
            trees,
            invalidation: InvalidationState::default(),
            namespace_flights: crate::ops::namespace::NamespaceFlights::new(),
            pagination_locks: DashMap::new(),
            rate_limit_until: std::sync::Mutex::new(None),
            test_callouts: test_rx.map(std::sync::Mutex::new),
        };
        runtime
            .publish_effects(&initialize_effects, runtime.resources.current_generation())
            .map_err(|error| BuildError::ProviderProtocol(error.to_string()))?;
        Ok(runtime)
    }

    pub fn shutdown(&self) -> Result<()> {
        self.instance.shutdown()
    }

    pub fn call_close_file(&self, handle: u64) -> Result<()> {
        self.instance.close_file(handle)
    }

    /// Test-only entry to drive provider effects from FUSE-path pagination
    /// harnesses without routing through a provider component.
    #[doc(hidden)]
    pub fn apply_effects_for_test(&self, effects: &wit_types::Effects, op_gen: u64) -> Result<()> {
        let now = clock::now_millis();
        let (prefixes, paths) = crate::effect_apply::EffectApplier::new(&self.resources)
            .apply(effects, op_gen, now)
            .map_err(|error| {
                EngineError::ProviderProtocol(format!("cache publication failed: {error}"))
            })?;
        self.record_view_invalidations(prefixes, paths);
        Ok(())
    }

    #[doc(hidden)]
    pub fn apply_not_found_negative(
        &self,
        path: &Path,
        maybe_id: Option<&wit_types::LogicalId>,
        op_gen: u64,
        now_millis: u64,
    ) -> Result<()> {
        let id_bytes = maybe_id.map(|id| ObjectId::from_wit(id).as_bytes().to_vec());
        self.resources
            .put_negative(
                path,
                id_bytes.as_deref(),
                op_gen,
                DYNAMIC_TTL_MILLIS,
                now_millis,
            )
            .map_err(|error| {
                EngineError::ProviderProtocol(format!("cache publication failed: {error}"))
            })?;
        Ok(())
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

    pub async fn call_timer_tick(&self) -> Result<()> {
        self.run_event(wit_types::ProviderEvent::TimerTick).await
    }

    /// Resolve a host-issued Git tree handle for the private namespace facade.
    pub(crate) fn tree_ref(&self, tree_ref: u64) -> Option<crate::tree_refs::TreeRef> {
        self.trees.resolve(tree_ref)
    }

    /// Serve the canonical bytes for `path` from the anchor-keyed object
    /// cache. When a `read-file` terminal answers `byte-source::canonical`, the
    /// tree resolves the longest covering anchor and returns those bytes
    /// without copying across the WIT. `None` when no stored anchor covers
    /// `path`.
    pub(crate) fn canonical_bytes_for(&self, path: &Path) -> Option<Vec<u8>> {
        self.resources
            .cached_canonical_for(path)
            .map(|canonical| canonical.bytes)
    }

    /// Read the full bytes of a stored blob for a blob-backed `read-file`
    /// terminal.
    pub(crate) fn read_blob_full(&self, blob_id: u64) -> Result<Vec<u8>> {
        let record =
            self.resources.blob.lookup_by_id(blob_id).ok_or_else(|| {
                EngineError::ProviderProtocol(format!("blob {blob_id} not found"))
            })?;
        let path = self.resources.blob.body_path(&record);
        std::fs::read(path)
            .map_err(|e| EngineError::ProviderProtocol(format!("read blob {blob_id}: {e}")))
    }

    pub(crate) fn publish_effects(&self, effects: &wit_types::Effects, op_gen: u64) -> Result<()> {
        let now = clock::now_millis();
        let (prefixes, paths) = crate::effect_apply::EffectApplier::new(&self.resources)
            .apply(effects, op_gen, now)
            .map_err(|error| {
                EngineError::ProviderProtocol(format!("cache publication failed: {error}"))
            })?;
        self.record_view_invalidations(prefixes, paths);
        Ok(())
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
