//! Provider registry: dynamic loading and lifecycle management for WASM providers.
//!
//! Owns the shared engine, extractor, and caches. Mounts are added and
//! removed at runtime through [`ProviderRegistry::add_mount`] and
//! [`ProviderRegistry::remove_mount`]; there is no startup directory scan.

use crate::cloner::GitCloner;
use crate::tools::archive::{ARCHIVE_TOOL_WASM, ArchiveExtractorComponent, DEFAULT_LIMITS};
use crate::{Artifact, BuildError, HostContext, Runtime, component_engine};
use omnifs_cache::Caches;
use omnifs_mount::materialize::{MaterializationMode, materialize};
use omnifs_mount::mounts::{Catalog, Resolved, Spec, spec_paths_in};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tracing::{debug, info, warn};

/// Registry of loaded WASM providers.
///
/// Instantiates providers on demand and manages their lifecycle including
/// per-mount timer-driven refresh tasks.
pub struct ProviderRegistry {
    engine: wasmtime::Engine,
    extractor: Arc<ArchiveExtractorComponent>,
    caches: Arc<Caches>,
    cloner: Arc<GitCloner>,
    context: HostContext,
    mounts: MountSupervisor,
}

impl ProviderRegistry {
    /// Build an empty registry: engine, archive extractor, and cache handles
    /// are created once here and shared across every mount added later.
    pub fn new(
        context: impl Into<HostContext>,
        cloner: Arc<GitCloner>,
    ) -> Result<Self, RegistryError> {
        let context = context.into();
        // Compiled component artifacts live with the rest of the host's state,
        // under `<cache>/wasm`, rather than a global per-user wasmtime cache.
        let wasm_cache = context.wasm_cache_dir();
        let engine = component_engine(Some(&wasm_cache), |config| {
            if let Some(strategy) = crate::provider_compiler_strategy() {
                config.strategy(strategy);
            }
        })
        .map_err(|e| RegistryError::RuntimeError(format!("provider engine init: {e}")))?;

        // One extractor (engine + parsed component + linker pre) shared
        // across every mount; the per-call sandbox lives on a fresh
        // `wasmtime::Store`. Shares the same on-disk artifact cache.
        let archive_tool_path = context.provider_path(ARCHIVE_TOOL_WASM);
        let extractor = Arc::new(
            ArchiveExtractorComponent::from_path(
                &archive_tool_path,
                DEFAULT_LIMITS,
                Some(&wasm_cache),
            )
            .map_err(|e| RegistryError::RuntimeError(format!("extractor init: {e}")))?,
        );

        // Global cache handles: a durable object database and a disposable view
        // database cleared + reopened on startup (Codex #5). Shared across all
        // provider runtimes; the object tier isolates mounts by keyspace, the
        // view tier by a path prefix.
        let caches = Caches::open(context.cache_dir())
            .map_err(|e| RegistryError::RuntimeError(format!("cache open: {e}")))?;

        Ok(Self {
            engine,
            extractor,
            caches,
            cloner,
            context,
            mounts: MountSupervisor::new(),
        })
    }

    /// Resolve and instantiate one mount, register it, and start its
    /// refresh timer (when the provider requests one) on `handle`.
    pub fn add_mount(
        &self,
        spec: Spec,
        handle: &tokio::runtime::Handle,
    ) -> Result<Arc<Runtime>, RegistryError> {
        self.mounts.add_mount(self, spec, handle)
    }

    /// Stop and unregister a mount: abort its timer, shut the provider
    /// down, and drop it from the instance map.
    pub fn remove_mount(&self, mount: &str) -> Result<(), RegistryError> {
        self.mounts.remove_mount(mount)
    }

    /// Host context this registry resolves mounts against.
    pub fn context(&self) -> &HostContext {
        &self.context
    }

    pub fn get(&self, mount: &str) -> Option<Arc<Runtime>> {
        self.mounts.get(mount)
    }

    pub fn mounts(&self) -> Vec<String> {
        self.mounts.mount_names()
    }

    pub fn runtime_entries(&self) -> Vec<(String, Arc<Runtime>)> {
        self.mounts.runtime_entries()
    }

    /// Returns the mount name of the root-mounted provider, if any.
    pub fn root_mount_name(&self) -> Option<String> {
        self.mounts.root_mount_name()
    }

    pub fn shutdown_all(&self) {
        self.mounts.shutdown_all();
    }

    /// Converge the running mount set to the desired state under
    /// `<config_dir>/mounts/*.json`.
    ///
    /// Desired specs are materialized (metadata, runtime capabilities, preopen
    /// rewriting) and fingerprinted; a spec that is new is added, one whose
    /// fingerprint changed is replaced, one that disappeared is removed, and one
    /// that fails to materialize or instantiate is recorded in
    /// [`ReconcileOutcome::failed`] without aborting the pass. `mode` selects
    /// host-direct preopens versus container-rewritten preopens.
    pub fn reconcile(
        &self,
        handle: &tokio::runtime::Handle,
        mode: MaterializationMode,
    ) -> ReconcileOutcome {
        let _guard = self.mounts.reconcile_guard();
        ReconcilePass::new(self, handle, mode).run()
    }
}

struct MountSupervisor {
    instances: parking_lot::RwLock<HashMap<String, Arc<Runtime>>>,
    root_mount: parking_lot::RwLock<Option<String>>,
    timer_shutdown: watch::Sender<bool>,
    timer_tasks: parking_lot::Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
    /// Per-mount materialized fingerprint, used by reconcile to detect a spec
    /// or provider-artifact change. Keyed by mount name.
    fingerprints: parking_lot::RwLock<HashMap<String, MountFingerprint>>,
    /// Serializes reconcile passes so concurrent triggers cannot race the
    /// add/remove sequence.
    reconcile_lock: parking_lot::Mutex<()>,
}

impl MountSupervisor {
    fn new() -> Self {
        let (timer_shutdown, _) = watch::channel(false);
        Self {
            instances: parking_lot::RwLock::new(HashMap::new()),
            root_mount: parking_lot::RwLock::new(None),
            timer_shutdown,
            timer_tasks: parking_lot::Mutex::new(HashMap::new()),
            fingerprints: parking_lot::RwLock::new(HashMap::new()),
            reconcile_lock: parking_lot::Mutex::new(()),
        }
    }

    fn add_mount(
        &self,
        registry: &ProviderRegistry,
        spec: Spec,
        handle: &tokio::runtime::Handle,
    ) -> Result<Arc<Runtime>, RegistryError> {
        omnifs_core::mount::Name::new(spec.mount.clone())
            .map_err(|error| RegistryError::ConfigError(format!("invalid mount name: {error}")))?;
        let mount = spec.mount.clone();
        if self.instances.read().contains_key(&mount) {
            return Err(RegistryError::DuplicateMount(mount));
        }

        let wasm_path = registry.context.provider_path(&spec.provider);
        if !wasm_path.exists() {
            return Err(RegistryError::ProviderNotFound(
                wasm_path.display().to_string(),
            ));
        }
        let resolved = resolve_mount_for_wasm(&wasm_path, spec)
            .map_err(|error| registry_error(&mount, error))?;
        let is_root = resolved.spec.root_mount;

        // Instantiation compiles WASM; keep it outside the instances lock.
        let runtime = Runtime::new(
            &registry.engine,
            &wasm_path,
            &resolved,
            registry.cloner.clone(),
            &registry.context,
            registry.extractor.clone(),
            &registry.caches,
        )
        .map_err(|error| registry_error(&mount, error))?;
        let runtime = Arc::new(runtime);

        // Claim the root binding before the instance becomes visible: a
        // concurrent root lookup must never observe "instance present, no
        // root mount" for a root-mounted provider, or it would materialize
        // the mount as a root *child* with an infinite-TTL dentry.
        let mut claimed_root = false;
        if is_root {
            let mut root = self.root_mount.write();
            if let Some(existing) = root.as_deref() {
                warn!(
                    mount = mount.as_str(),
                    existing, "multiple root_mount providers; ignoring root_mount for this one"
                );
            } else {
                *root = Some(mount.clone());
                claimed_root = true;
            }
        }
        {
            let mut instances = self.instances.write();
            if instances.contains_key(&mount) {
                if claimed_root {
                    *self.root_mount.write() = None;
                }
                return Err(RegistryError::DuplicateMount(mount));
            }
            instances.insert(mount.clone(), Arc::clone(&runtime));
        }
        self.start_timer(&mount, &runtime, handle);
        info!(mount = mount.as_str(), root = is_root, "loaded provider");
        Ok(runtime)
    }

    fn remove_mount(&self, mount: &str) -> Result<(), RegistryError> {
        let Some(runtime) = self.instances.write().remove(mount) else {
            return Err(RegistryError::MountNotFound(mount.to_string()));
        };
        if let Some(task) = self.timer_tasks.lock().remove(mount) {
            task.abort();
        }
        {
            let mut root = self.root_mount.write();
            if root.as_deref() == Some(mount) {
                *root = None;
            }
        }
        if let Err(e) = runtime.shutdown() {
            warn!(mount, error = %e, "shutdown failed");
        }
        info!(mount, "removed provider");
        Ok(())
    }

    fn load_work(
        &self,
        registry: &ProviderRegistry,
        handle: &tokio::runtime::Handle,
        work: LoadWork,
    ) -> LoadResult {
        let LoadWork {
            spec,
            mount,
            wasm_path,
            fingerprint,
            running,
            reason,
        } = work;
        let started = Instant::now();
        if running {
            let _ = self.remove_mount(&mount);
        }
        match self.add_mount(registry, spec, handle) {
            Ok(_) => {
                self.fingerprints.write().insert(mount.clone(), fingerprint);
                if running {
                    info!(
                        mount = mount.as_str(),
                        provider = %wasm_path.display(),
                        reason,
                        duration_ms = started.elapsed().as_millis(),
                        "reconcile updated mount"
                    );
                    LoadResult::Updated { mount }
                } else {
                    info!(
                        mount = mount.as_str(),
                        provider = %wasm_path.display(),
                        reason,
                        duration_ms = started.elapsed().as_millis(),
                        "reconcile added mount"
                    );
                    LoadResult::Added { mount }
                }
            },
            Err(error) => {
                self.fingerprints.write().remove(&mount);
                warn!(
                    mount = mount.as_str(),
                    provider = %wasm_path.display(),
                    reason,
                    duration_ms = started.elapsed().as_millis(),
                    error = %error,
                    "reconcile failed to load mount"
                );
                LoadResult::Failed {
                    mount,
                    reason: error.to_string(),
                }
            },
        }
    }

    fn get(&self, mount: &str) -> Option<Arc<Runtime>> {
        self.instances.read().get(mount).cloned()
    }

    fn mount_names(&self) -> Vec<String> {
        self.instances.read().keys().cloned().collect()
    }

    fn runtime_entries(&self) -> Vec<(String, Arc<Runtime>)> {
        self.instances
            .read()
            .iter()
            .map(|(mount, runtime)| (mount.clone(), Arc::clone(runtime)))
            .collect()
    }

    fn root_mount_name(&self) -> Option<String> {
        self.root_mount.read().clone()
    }

    fn is_running(&self, mount: &str) -> bool {
        self.instances.read().contains_key(mount)
    }

    fn fingerprint(&self, mount: &str) -> Option<MountFingerprint> {
        self.fingerprints.read().get(mount).copied()
    }

    fn remove_fingerprint(&self, mount: &str) {
        self.fingerprints.write().remove(mount);
    }

    fn shutdown_all(&self) {
        let _ = self.timer_shutdown.send(true);
        for (_, task) in self.timer_tasks.lock().drain() {
            task.abort();
        }
        for (mount, runtime) in self.instances.read().iter() {
            if let Err(e) = runtime.shutdown() {
                warn!(mount, error = %e, "shutdown failed");
            }
        }
    }

    fn reconcile_guard(&self) -> parking_lot::MutexGuard<'_, ()> {
        self.reconcile_lock.lock()
    }

    fn start_timer(&self, mount: &str, runtime: &Arc<Runtime>, handle: &tokio::runtime::Handle) {
        let interval_secs = runtime.requested_capabilities().refresh_interval_secs;
        if interval_secs == 0 {
            return;
        }

        let mount = mount.to_string();
        let runtime = Arc::clone(runtime);
        let mut shutdown = self.timer_shutdown.subscribe();
        let task = handle.spawn({
            let mount = mount.clone();
            async move {
                if *shutdown.borrow_and_update() {
                    return;
                }
                let mut interval =
                    tokio::time::interval(Duration::from_secs(u64::from(interval_secs)));
                loop {
                    tokio::select! {
                        _ = interval.tick() => {
                            if let Err(e) = runtime.call_timer_tick().await {
                                debug!(mount = mount.as_str(), error = %e, "provider timer tick failed");
                            }
                        }
                        changed = shutdown.changed() => {
                            if changed.is_ok() {
                                break;
                            }
                        }
                    }
                }
            }
        });
        self.timer_tasks.lock().insert(mount, task);
    }
}

struct ReconcilePass<'a> {
    registry: &'a ProviderRegistry,
    handle: &'a tokio::runtime::Handle,
    mode: MaterializationMode,
    catalog: Catalog,
    desired: HashSet<String>,
    outcome: ReconcileOutcome,
    started: Instant,
}

impl<'a> ReconcilePass<'a> {
    fn new(
        registry: &'a ProviderRegistry,
        handle: &'a tokio::runtime::Handle,
        mode: MaterializationMode,
    ) -> Self {
        Self {
            registry,
            handle,
            mode,
            catalog: Catalog::new(
                registry.context.mounts_dir(),
                registry.context.providers_dir(),
            ),
            desired: HashSet::new(),
            outcome: ReconcileOutcome::default(),
            started: Instant::now(),
        }
    }

    fn run(mut self) -> ReconcileOutcome {
        let paths = match spec_paths_in(self.catalog.mounts_dir()) {
            Ok(paths) => paths,
            Err(error) => {
                self.outcome.failed.push(MountFailure {
                    mount: self.catalog.mounts_dir().display().to_string(),
                    reason: format!("scan mounts dir: {error}"),
                });
                return self.outcome;
            },
        };

        // Phase 1 (serial, cheap): materialize, fingerprint, and decide. Settles
        // unchanged mounts and materialize/duplicate failures here; only the
        // compile-heavy loads carry into phase 2.
        let mut work = Vec::new();
        for path in &paths {
            if let Some(item) = self.plan_path(path) {
                work.push(item);
            }
        }

        // Phase 2 (parallel): compile + instantiate the planned mounts.
        for result in self.load_in_parallel(work) {
            self.record_load(result);
        }

        self.remove_stale_mounts();
        info!(
            added = self.outcome.added.len(),
            updated = self.outcome.updated.len(),
            removed = self.outcome.removed.len(),
            failed = self.outcome.failed.len(),
            duration_ms = self.started.elapsed().as_millis(),
            "reconcile completed"
        );
        self.outcome
    }

    /// Decide what a spec path needs. Records unchanged mounts and
    /// materialize/duplicate failures into the outcome directly; returns a
    /// `LoadWork` only for mounts that must be (re)compiled.
    fn plan_path(&mut self, path: &Path) -> Option<LoadWork> {
        let materialized = self.materialized_spec(path)?;
        let mount = materialized.mount.clone();
        // Distinct mount names are required: two specs claiming one name would
        // race in the parallel load, and it is a misconfiguration regardless.
        if !self.desired.insert(mount.clone()) {
            self.outcome.failed.push(MountFailure {
                mount,
                reason: format!("duplicate mount name from {}", path.display()),
            });
            return None;
        }

        let wasm_path = self.registry.context.provider_path(&materialized.provider);
        let fingerprint = mount_fingerprint(&materialized, &wasm_path);
        let running = self.registry.mounts.is_running(&mount);
        let prior_fingerprint = self.registry.mounts.fingerprint(&mount);
        if running && prior_fingerprint == Some(fingerprint) {
            debug!(
                mount = mount.as_str(),
                provider = materialized.provider.as_str(),
                "reconcile mount unchanged"
            );
            return None;
        }

        let reason = prior_fingerprint.map_or("new", |prior| fingerprint.reason_since(prior));
        Some(LoadWork {
            spec: materialized,
            mount,
            wasm_path,
            fingerprint,
            running,
            reason,
        })
    }

    /// Compile and register the planned mounts. Compilation dominates reconcile
    /// wall-time (a cold Cranelift compile is seconds per provider), and the
    /// loads are independent across distinct mount names (`plan_path` enforces
    /// that), so they run on a pool of worker threads bounded by the core count.
    /// Empty and single-item work skips the thread setup.
    ///
    /// Uses [`std::thread::scope`], not tokio tasks or the blocking pool: the
    /// work is CPU-bound WASM compile plus synchronous WASI init, not async I/O.
    /// Scoped OS threads carry no tokio handle, so `without_tokio_handle` in
    /// [`crate::instance`] runs instantiation inline rather than spawning an
    /// escape thread per mount. Tokio blocking-pool threads do have a handle and
    /// would regress bulk reconcile to O(mounts) extra threads. The async
    /// boundary already sits at the daemon, which wraps the full reconcile in
    /// `spawn_blocking` (`omnifs-daemon` `server.rs`). Revisit if/when the WASI
    /// path moves to `add_to_linker_async`.
    fn load_in_parallel(&self, work: Vec<LoadWork>) -> Vec<LoadResult> {
        let registry = self.registry;
        let handle = self.handle;
        if work.len() <= 1 {
            return work
                .into_iter()
                .map(|item| registry.mounts.load_work(registry, handle, item))
                .collect();
        }

        let workers = std::thread::available_parallelism()
            .map(std::num::NonZero::get)
            .unwrap_or(4)
            .min(work.len());

        // Round-robin the work into one owned bucket per worker so each thread
        // drains its own bucket with no shared mutable state.
        let mut buckets: Vec<Vec<LoadWork>> = (0..workers).map(|_| Vec::new()).collect();
        for (i, item) in work.into_iter().enumerate() {
            buckets[i % workers].push(item);
        }

        std::thread::scope(|scope| {
            let handles: Vec<_> = buckets
                .into_iter()
                .map(|bucket| {
                    scope.spawn(move || {
                        bucket
                            .into_iter()
                            .map(|item| registry.mounts.load_work(registry, handle, item))
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            handles
                .into_iter()
                .flat_map(|h| h.join().expect("reconcile worker thread panicked"))
                .collect()
        })
    }

    fn record_load(&mut self, result: LoadResult) {
        match result {
            LoadResult::Added { mount } => self.outcome.added.push(mount),
            LoadResult::Updated { mount } => self.outcome.updated.push(mount),
            LoadResult::Failed { mount, reason } => {
                self.outcome.failed.push(MountFailure { mount, reason });
            },
        }
    }

    fn materialized_spec(&mut self, path: &Path) -> Option<Spec> {
        let spec = match Spec::from_file(path) {
            Ok(spec) => spec,
            Err(error) => {
                self.outcome.failed.push(MountFailure {
                    mount: path.display().to_string(),
                    reason: error.to_string(),
                });
                return None;
            },
        };
        match materialize(spec, &self.catalog, self.mode) {
            Ok(materialized) => Some(materialized.spec),
            Err(error) => {
                self.outcome.failed.push(MountFailure {
                    mount: path.display().to_string(),
                    reason: error.to_string(),
                });
                None
            },
        }
    }

    fn remove_stale_mounts(&mut self) {
        for mount in self.registry.mounts.mount_names() {
            if !self.desired.contains(&mount) {
                let mount_started = Instant::now();
                if self.registry.mounts.remove_mount(&mount).is_ok() {
                    info!(
                        mount = mount.as_str(),
                        duration_ms = mount_started.elapsed().as_millis(),
                        "reconcile removed mount"
                    );
                    self.outcome.removed.push(mount.clone());
                }
                self.registry.mounts.remove_fingerprint(&mount);
            }
        }
    }
}

/// A planned mount that needs (re)compilation, produced by `plan_path`.
struct LoadWork {
    spec: Spec,
    mount: String,
    wasm_path: PathBuf,
    fingerprint: MountFingerprint,
    /// Whether a prior instance is being replaced (update) versus a fresh add.
    running: bool,
    reason: &'static str,
}

/// The outcome of loading one [`LoadWork`], folded back into the reconcile
/// outcome by `record_load`.
enum LoadResult {
    Added { mount: String },
    Updated { mount: String },
    Failed { mount: String, reason: String },
}

/// One mount that did not converge during [`ProviderRegistry::reconcile`].
#[derive(Debug, Clone)]
pub struct MountFailure {
    /// Mount name, or the spec path when the name could not be parsed.
    pub mount: String,
    pub reason: String,
}

/// What a reconcile pass changed. Host-local; the daemon maps it to the
/// control-API report type.
#[derive(Debug, Default)]
pub struct ReconcileOutcome {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub updated: Vec<String>,
    pub failed: Vec<MountFailure>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MountFingerprint {
    spec: u64,
    artifact: u64,
}

impl MountFingerprint {
    fn reason_since(self, prior: Self) -> &'static str {
        match (self.spec != prior.spec, self.artifact != prior.artifact) {
            (true, true) => "config+provider",
            (true, false) => "config",
            (false, true) => "provider",
            (false, false) => "unchanged",
        }
    }
}

/// Fingerprint a materialized spec plus its provider artifact, so a reconcile
/// detects both config edits and a swapped-out provider binary. The provider
/// stamp uses file length and mtime rather than a content hash to keep the pass
/// cheap; a rebuilt provider changes both.
fn mount_fingerprint(spec: &Spec, wasm_path: &Path) -> MountFingerprint {
    MountFingerprint {
        spec: spec_fingerprint(spec),
        artifact: artifact_fingerprint(wasm_path),
    }
}

fn spec_fingerprint(spec: &Spec) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    if let Ok(bytes) = serde_json::to_vec(spec) {
        bytes.hash(&mut hasher);
    }
    hasher.finish()
}

fn artifact_fingerprint(wasm_path: &Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    if let Ok(meta) = std::fs::metadata(wasm_path) {
        meta.len().hash(&mut hasher);
        if let Ok(modified) = meta.modified()
            && let Ok(since_epoch) = modified.duration_since(std::time::UNIX_EPOCH)
        {
            since_epoch.as_nanos().hash(&mut hasher);
        }
    }
    hasher.finish()
}

fn registry_error(mount: &str, error: BuildError) -> RegistryError {
    match error {
        BuildError::InvalidConfig(message) => {
            RegistryError::ConfigError(format!("mount {mount}: {message}"))
        },
        other => RegistryError::RuntimeError(other.to_string()),
    }
}

fn resolve_mount_for_wasm(wasm_path: &Path, config: Spec) -> Result<Resolved, BuildError> {
    let fallback_provider_id = wasm_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(&config.mount)
        .to_string();
    let metadata = Artifact::load(wasm_path)
        .and_then(|artifact| artifact.metadata())
        .map_err(BuildError::InvalidConfig)?;
    config
        .into_resolved(fallback_provider_id, metadata.as_ref())
        .map_err(|error| BuildError::InvalidConfig(error.to_string()))
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("config error: {0}")]
    ConfigError(String),
    #[error("mount `{0}` is already loaded")]
    DuplicateMount(String),
    #[error("mount `{0}` is not loaded")]
    MountNotFound(String),
    #[error("provider not found: {0}")]
    ProviderNotFound(String),
    #[error("runtime error: {0}")]
    RuntimeError(String),
}

#[cfg(test)]
mod tests {
    use super::{ProviderRegistry, RegistryError};
    use crate::HostContext;
    use crate::cloner::GitCloner;
    use crate::tools::archive::ARCHIVE_TOOL_WASM;
    use omnifs_mount::materialize::MaterializationMode;
    use omnifs_mount::mounts::Spec;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    fn wasm_artifact_path(file_name: &str) -> PathBuf {
        let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("host crate must have a workspace parent")
            .parent()
            .expect("workspace root must exist");
        workspace_root
            .join("target")
            .join("wasm32-wasip2")
            .join("release")
            .join(file_name)
    }

    fn test_provider_wasm_path() -> PathBuf {
        wasm_artifact_path("test_provider.wasm")
    }

    fn archive_tool_wasm_path() -> PathBuf {
        wasm_artifact_path(ARCHIVE_TOOL_WASM)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn add_mount_rejects_invalid_provider_config() {
        // The test provider's embedded manifest declares a configSchema
        // (additionalProperties: false, no properties), so the host's
        // validate_instance_config rejects a mount config with extra fields.
        let config_dir = tempfile::tempdir().expect("temp config dir");
        let cache_dir = tempfile::tempdir().expect("temp cache dir");
        let providers_dir = tempfile::tempdir().expect("temp providers dir");
        let paths = omnifs_home::WorkspaceLayout::under_root(config_dir.path());

        let base_wasm = test_provider_wasm_path();
        assert!(
            base_wasm.exists(),
            "test provider missing at {}. Run `just providers-build` first.",
            base_wasm.display()
        );
        std::fs::copy(&base_wasm, providers_dir.path().join("test_provider.wasm"))
            .expect("copy test provider");
        let archive_tool_wasm = archive_tool_wasm_path();
        assert!(
            archive_tool_wasm.exists(),
            "archive tool missing at {}. Run `just providers-build` first.",
            archive_tool_wasm.display()
        );
        std::fs::copy(
            &archive_tool_wasm,
            providers_dir.path().join(ARCHIVE_TOOL_WASM),
        )
        .expect("copy archive tool");

        let cloner = Arc::new(GitCloner::new(cache_dir.path().join("clones")));
        let registry = ProviderRegistry::new(
            HostContext::new(
                cache_dir.path(),
                &paths.config_dir,
                providers_dir.path(),
                &paths.credentials_file,
            ),
            cloner,
        )
        .expect("registry init");

        let spec = Spec::parse(
            r#"{
                "provider": "test_provider.wasm",
                "mount": "test",
                "config": {
                    "unexpected": true
                }
            }"#,
        )
        .expect("parse spec");

        match registry.add_mount(spec, &tokio::runtime::Handle::current()) {
            Err(RegistryError::ConfigError(message)) => {
                assert!(message.contains("failed validation"));
                assert!(message.contains("mount test"));
            },
            Err(other) => panic!("expected config error, got {other}"),
            Ok(_) => panic!("expected invalid provider config to be rejected"),
        }
        assert!(registry.mounts().is_empty());
    }

    /// The daemon contract backstop (PLAN.md slice 4): a mount whose stamped
    /// contract does not match the live provider contract is refused at
    /// reconcile and surfaces as a `MountFailure`, so the daemon never serves a
    /// contract the spec was not written against. Guards the "daemon refuses a
    /// hand-edited drifted spec" guarantee, the one slice-4 outcome no other
    /// test exercises.
    #[tokio::test(flavor = "multi_thread")]
    async fn reconcile_refuses_contract_drifted_mount() {
        let config_dir = tempfile::tempdir().expect("temp config dir");
        let cache_dir = tempfile::tempdir().expect("temp cache dir");
        let providers_dir = tempfile::tempdir().expect("temp providers dir");
        let paths = omnifs_home::WorkspaceLayout::under_root(config_dir.path());

        // `ProviderRegistry::new` builds the archive extractor from this WASM,
        // so it must be present. The drifted mount itself uses the built-in db
        // provider manifest and is rejected before its WASM is ever loaded.
        let archive_tool_wasm = archive_tool_wasm_path();
        assert!(
            archive_tool_wasm.exists(),
            "archive tool missing at {}. Run `just providers-build` first.",
            archive_tool_wasm.display()
        );
        std::fs::copy(
            &archive_tool_wasm,
            providers_dir.path().join(ARCHIVE_TOOL_WASM),
        )
        .expect("copy archive tool");

        // A db mount whose stamped contract carries a config field the live
        // built-in manifest does not, so the two contract hashes cannot match.
        let mounts_dir = paths.config_dir.join("mounts");
        std::fs::create_dir_all(&mounts_dir).expect("create mounts dir");
        std::fs::write(
            mounts_dir.join("db.json"),
            r#"{
                "provider": "omnifs_provider_db.wasm",
                "mount": "db",
                "config": {"database_type": "sqlite", "path": "/data/chinook.sqlite"},
                "contract": {
                    "config_fields": [{"name": "__drift_probe__", "required": true}],
                    "capabilities": [],
                    "auth_scheme": "__drift_probe_auth__",
                    "provider_version": "0.0.0-drift"
                }
            }"#,
        )
        .expect("write drifted spec");

        let cloner = Arc::new(GitCloner::new(cache_dir.path().join("clones")));
        let registry = ProviderRegistry::new(
            HostContext::new(
                cache_dir.path(),
                &paths.config_dir,
                providers_dir.path(),
                &paths.credentials_file,
            ),
            cloner,
        )
        .expect("registry init");

        let outcome = registry.reconcile(
            &tokio::runtime::Handle::current(),
            MaterializationMode::Docker,
        );

        // The drifted mount is refused, not served, and the failure names the
        // contract mismatch so it is actionable in `omnifs status`.
        assert!(
            registry.mounts().is_empty(),
            "a contract-drifted mount must not be served"
        );
        assert!(outcome.added.is_empty(), "no mount should have been added");
        let failure = outcome
            .failed
            .iter()
            .find(|failure| failure.reason.contains("contract mismatch"))
            .unwrap_or_else(|| {
                panic!(
                    "expected a contract-mismatch failure, got: {:?}",
                    outcome.failed
                )
            });
        assert!(
            failure.reason.contains("mount `db`"),
            "failure should name the drifted mount, got: {}",
            failure.reason
        );
    }
}
