//! Provider registry: dynamic loading and lifecycle management for WASM providers.
//!
//! Owns the shared engine and caches. Mounts are added and
//! removed at runtime through [`ProviderRegistry::add_mount`] and
//! [`ProviderRegistry::remove_mount`]; there is no startup directory scan.

use crate::cloner::GitCloner;
use crate::{BuildError, HostContext, Runtime, component_engine};
use omnifs_cache::Caches;
use omnifs_mount::materialize::{MaterializationMode, materialize};
use omnifs_mount::mounts::{Registry, Spec};
use omnifs_provider::Catalog;
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
    caches: Arc<Caches>,
    cloner: Arc<GitCloner>,
    context: HostContext,
    instances: parking_lot::RwLock<HashMap<String, Arc<Runtime>>>,
    root_mount: parking_lot::RwLock<Option<String>>,
    timer_shutdown: watch::Sender<bool>,
    timer_tasks: parking_lot::Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
    /// Per-mount materialized spec fingerprint, used by reconcile to detect a
    /// spec change. The pinned `ProviderRef` is part of the spec, so an artifact
    /// swap shows up as a spec change without re-hashing the WASM.
    fingerprints: parking_lot::RwLock<HashMap<String, u64>>,
    /// Serializes reconcile passes so concurrent triggers cannot race the
    /// add/remove sequence.
    reconcile_lock: parking_lot::Mutex<()>,
}

impl ProviderRegistry {
    /// Build an empty registry: engine and cache handles are created once
    /// here and shared across every mount added later.
    pub fn new(context: HostContext, cloner: Arc<GitCloner>) -> Result<Self, RegistryError> {
        // Compiled component artifacts live with the rest of the host's state,
        // under `<cache>/wasm`, rather than a global per-user wasmtime cache.
        let wasm_cache = context.wasm_cache_dir();
        let engine = component_engine(Some(&wasm_cache), |config| {
            if let Some(strategy) = crate::provider_compiler_strategy() {
                config.strategy(strategy);
            }
        })
        .map_err(|e| RegistryError::RuntimeError(format!("provider engine init: {e}")))?;

        // Global cache handles: a durable object database and a disposable view
        // database cleared + reopened on startup (Codex #5). Shared across all
        // provider runtimes; the object tier isolates mounts by keyspace, the
        // view tier by a path prefix.
        let caches = Caches::open(context.cache_dir())
            .map_err(|e| RegistryError::RuntimeError(format!("cache open: {e}")))?;

        let (timer_shutdown, _) = watch::channel(false);
        Ok(Self {
            engine,
            caches,
            cloner,
            context,
            instances: parking_lot::RwLock::new(HashMap::new()),
            root_mount: parking_lot::RwLock::new(None),
            timer_shutdown,
            timer_tasks: parking_lot::Mutex::new(HashMap::new()),
            fingerprints: parking_lot::RwLock::new(HashMap::new()),
            reconcile_lock: parking_lot::Mutex::new(()),
        })
    }

    /// Resolve and instantiate one mount, register it, and start its
    /// refresh timer (when the provider requests one) on `handle`.
    pub fn add_mount(
        &self,
        spec: &Spec,
        handle: &tokio::runtime::Handle,
    ) -> Result<Arc<Runtime>, RegistryError> {
        let mount = spec.mount.clone();
        if self.instances.read().contains_key(&mount) {
            return Err(RegistryError::DuplicateMount(mount));
        }
        let built = self.build_mount(spec, false)?;
        self.publish_new_mount(built, handle)
    }

    /// Test-support twin of [`ProviderRegistry::add_mount`]: the mount's
    /// runtime is built with [`Runtime::new_for_callout_tests`], so HTTP and
    /// blob-fetch callouts suspend until the test answers them through
    /// [`Runtime::try_recv_test_callout`]. Lets a live frontend test park a
    /// provider read on a slow upstream the test itself controls.
    #[doc(hidden)]
    pub fn add_mount_for_callout_tests(
        &self,
        spec: &Spec,
        handle: &tokio::runtime::Handle,
    ) -> Result<Arc<Runtime>, RegistryError> {
        let mount = spec.mount.clone();
        if self.instances.read().contains_key(&mount) {
            return Err(RegistryError::DuplicateMount(mount));
        }
        let built = self.build_mount(spec, true)?;
        self.publish_new_mount(built, handle)
    }

    fn build_mount(
        &self,
        spec: &Spec,
        capture_test_callouts: bool,
    ) -> Result<BuiltMount, RegistryError> {
        omnifs_core::mount::Name::new(spec.mount.clone())
            .map_err(|error| RegistryError::ConfigError(format!("invalid mount name: {error}")))?;
        let mount = spec.mount.clone();
        let wasm_path = self.context.provider_path_by_id(&spec.provider.id);
        if !wasm_path.exists() {
            return Err(RegistryError::ProviderNotFound(
                wasm_path.display().to_string(),
            ));
        }
        let is_root = spec.root_mount;

        // Instantiation compiles WASM; keep it outside the instances lock.
        let runtime = if capture_test_callouts {
            Runtime::new_for_callout_tests(
                &self.engine,
                &wasm_path,
                spec,
                self.cloner.clone(),
                &self.context,
                &self.caches,
            )
        } else {
            Runtime::new(
                &self.engine,
                &wasm_path,
                spec,
                self.cloner.clone(),
                &self.context,
                &self.caches,
            )
        }
        .map_err(|error| registry_error(&mount, error))?;
        Ok(BuiltMount {
            mount,
            is_root,
            runtime: Arc::new(runtime),
        })
    }

    fn publish_new_mount(
        &self,
        built: BuiltMount,
        handle: &tokio::runtime::Handle,
    ) -> Result<Arc<Runtime>, RegistryError> {
        let BuiltMount {
            mount,
            is_root,
            runtime,
        } = built;
        // Claim the root binding before the instance becomes visible: a
        // concurrent root lookup must never observe "instance present, no
        // root mount" for a root-mounted provider, or it would materialize
        // the mount as a root child with an infinite-TTL dentry.
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

    fn replace_mount(
        &self,
        built: BuiltMount,
        handle: &tokio::runtime::Handle,
    ) -> Result<(), RegistryError> {
        let BuiltMount {
            mount,
            is_root,
            runtime,
        } = built;

        if let Some(task) = self.timer_tasks.lock().remove(&mount) {
            task.abort();
        }

        let mut claimed_root = false;
        {
            let mut root = self.root_mount.write();
            if root.as_deref() == Some(mount.as_str()) {
                *root = None;
            }
            if is_root {
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
        }

        let old_runtime = {
            let mut instances = self.instances.write();
            let Some(old_runtime) = instances.insert(mount.clone(), Arc::clone(&runtime)) else {
                if claimed_root {
                    *self.root_mount.write() = None;
                }
                return Err(RegistryError::MountNotFound(mount));
            };
            old_runtime
        };

        self.start_timer(&mount, &runtime, handle);
        if let Err(error) = old_runtime.shutdown() {
            warn!(mount = mount.as_str(), error = %error, "shutdown failed");
        }
        Ok(())
    }

    /// Stop and unregister a mount: abort its timer, shut the provider
    /// down, and drop it from the instance map.
    pub fn remove_mount(&self, mount: &str) -> Result<(), RegistryError> {
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

    /// Host context this registry resolves mounts against.
    pub fn context(&self) -> &HostContext {
        &self.context
    }

    pub fn get(&self, mount: &str) -> Option<Arc<Runtime>> {
        self.instances.read().get(mount).cloned()
    }

    pub fn mounts(&self) -> Vec<String> {
        self.instances.read().keys().cloned().collect()
    }

    pub fn runtime_entries(&self) -> Vec<(String, Arc<Runtime>)> {
        self.instances
            .read()
            .iter()
            .map(|(mount, runtime)| (mount.clone(), Arc::clone(runtime)))
            .collect()
    }

    /// Returns the mount name of the root-mounted provider, if any.
    pub fn root_mount_name(&self) -> Option<String> {
        self.root_mount.read().clone()
    }

    pub fn shutdown_all(&self) {
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
        self: &Arc<Self>,
        handle: &tokio::runtime::Handle,
        mode: MaterializationMode,
    ) -> ReconcileOutcome {
        let _guard = self.reconcile_lock.lock();
        ReconcilePass::new(self, handle, mode).run()
    }

    fn build_work(&self, work: LoadWork) -> LoadResult {
        let LoadWork {
            spec,
            mount,
            wasm_path,
            fingerprint,
            running,
            reason,
        } = work;
        let started = Instant::now();
        match self.build_mount(&spec, false) {
            Ok(built) => LoadResult::Ready {
                mount,
                wasm_path,
                fingerprint,
                running,
                reason,
                duration: started.elapsed(),
                built,
            },
            Err(error) => {
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

    fn is_running(&self, mount: &str) -> bool {
        self.instances.read().contains_key(mount)
    }

    fn fingerprint(&self, mount: &str) -> Option<u64> {
        self.fingerprints.read().get(mount).copied()
    }

    fn remove_fingerprint(&self, mount: &str) {
        self.fingerprints.write().remove(mount);
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
    registry: &'a Arc<ProviderRegistry>,
    handle: &'a tokio::runtime::Handle,
    mode: MaterializationMode,
    providers: Catalog,
    desired: HashSet<String>,
    outcome: ReconcileOutcome,
    started: Instant,
}

impl<'a> ReconcilePass<'a> {
    fn new(
        registry: &'a Arc<ProviderRegistry>,
        handle: &'a tokio::runtime::Handle,
        mode: MaterializationMode,
    ) -> Self {
        Self {
            registry,
            handle,
            mode,
            providers: Catalog::open(registry.context.providers_dir()),
            desired: HashSet::new(),
            outcome: ReconcileOutcome::default(),
            started: Instant::now(),
        }
    }

    fn run(mut self) -> ReconcileOutcome {
        // Desired state is read fresh from disk each pass through the shared
        // mount Registry; the reconcile_lock serializes passes so this snapshot
        // is coherent for the duration of the pass.
        let registry = match Registry::load(self.registry.context.mounts_dir()) {
            Ok(registry) => registry,
            Err(error) => {
                self.outcome.failed.push(MountFailure {
                    mount: self.registry.context.mounts_dir().display().to_string(),
                    reason: format!("scan mounts dir: {error}"),
                });
                return self.outcome;
            },
        };

        // A spec that fails to parse or carries an invalid mount name is
        // recorded and skipped; it never aborts the pass or disturbs a running
        // mount.
        for failure in registry.failures() {
            self.outcome.failed.push(MountFailure {
                mount: failure.path.display().to_string(),
                reason: failure.error.to_string(),
            });
        }

        // Phase 1 (serial, cheap): materialize, fingerprint, and decide. Settles
        // unchanged mounts and materialize/duplicate failures here; only the
        // compile-heavy loads carry into phase 2.
        let mut work = Vec::new();
        for (name, spec) in registry.iter() {
            if let Some(item) = self.plan_spec(spec.clone(), &registry.spec_path(name)) {
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

    /// Decide what a desired spec needs. Records unchanged mounts and
    /// materialize/duplicate failures into the outcome directly; returns a
    /// `LoadWork` only for mounts that must be (re)compiled. `path` is the
    /// spec's on-disk file, used only for failure messages.
    fn plan_spec(&mut self, spec: Spec, path: &Path) -> Option<LoadWork> {
        let materialized = match materialize(spec, &self.providers, self.mode) {
            Ok(materialized) => materialized.into_spec(),
            Err(error) => {
                self.outcome.failed.push(MountFailure {
                    mount: path.display().to_string(),
                    reason: error.to_string(),
                });
                return None;
            },
        };
        let mount = materialized.mount.clone();
        // The mount Registry guarantees one spec per mount name (a spec must live
        // at `<mount>.json`, and a duplicate or misnamed file is a load failure),
        // so `desired` only tracks the set of names for stale-mount removal.
        self.desired.insert(mount.clone());

        let wasm_path = self
            .registry
            .context
            .provider_path_by_id(&materialized.provider.id);
        let fingerprint = mount_fingerprint(&materialized);
        let running = self.registry.is_running(&mount);
        let prior_fingerprint = self.registry.fingerprint(&mount);
        if running && prior_fingerprint == Some(fingerprint) {
            debug!(
                mount = mount.as_str(),
                provider = materialized.provider.meta.name.as_str(),
                "reconcile mount unchanged"
            );
            return None;
        }

        let reason = if prior_fingerprint.is_some() {
            "config"
        } else {
            "new"
        };
        Some(LoadWork {
            spec: materialized,
            mount,
            wasm_path,
            fingerprint,
            running,
            reason,
        })
    }

    /// Compile the planned mounts. Compilation dominates reconcile wall-time,
    /// and the loads are independent across distinct mount names (`plan_path`
    /// enforces that), so they run on Tokio's blocking pool. Publication
    /// happens later in spec-path order.
    /// Empty and single-item work skips the task setup.
    fn load_in_parallel(&self, work: Vec<LoadWork>) -> Vec<LoadResult> {
        if work.len() <= 1 {
            return work
                .into_iter()
                .map(|item| self.registry.build_work(item))
                .collect();
        }

        let expected = work.len();
        let (tx, rx) = std::sync::mpsc::channel();
        for (index, item) in work.into_iter().enumerate() {
            let mount = item.mount.clone();
            let registry = Arc::clone(self.registry);
            let tx = tx.clone();
            self.handle.spawn_blocking(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    registry.build_work(item)
                }))
                .unwrap_or_else(|_| LoadResult::Failed {
                    mount,
                    reason: "reconcile load task panicked".to_string(),
                });
                let _ = tx.send((index, result));
            });
        }
        drop(tx);

        let mut results = Vec::with_capacity(expected);
        while results.len() < expected {
            let Ok(result) = rx.recv() else {
                break;
            };
            results.push(result);
        }
        results.sort_by_key(|(index, _)| *index);
        results.into_iter().map(|(_, result)| result).collect()
    }

    fn record_load(&mut self, result: LoadResult) {
        match result {
            LoadResult::Ready {
                mount,
                wasm_path,
                fingerprint,
                running,
                reason,
                duration,
                built,
            } => {
                let apply_started = Instant::now();
                let applied = if running {
                    self.registry.replace_mount(built, self.handle)
                } else {
                    self.registry
                        .publish_new_mount(built, self.handle)
                        .map(|_| ())
                };
                match applied {
                    Ok(()) => {
                        self.registry
                            .fingerprints
                            .write()
                            .insert(mount.clone(), fingerprint);
                        if running {
                            info!(
                                mount = mount.as_str(),
                                provider = %wasm_path.display(),
                                reason,
                                duration_ms = (duration + apply_started.elapsed()).as_millis(),
                                "reconcile updated mount"
                            );
                            self.outcome.updated.push(mount);
                        } else {
                            info!(
                                mount = mount.as_str(),
                                provider = %wasm_path.display(),
                                reason,
                                duration_ms = (duration + apply_started.elapsed()).as_millis(),
                                "reconcile added mount"
                            );
                            self.outcome.added.push(mount);
                        }
                    },
                    Err(error) => {
                        self.outcome.failed.push(MountFailure {
                            mount,
                            reason: error.to_string(),
                        });
                    },
                }
            },
            LoadResult::Failed { mount, reason } => {
                self.outcome.failed.push(MountFailure { mount, reason });
            },
        }
    }

    fn remove_stale_mounts(&mut self) {
        for mount in self.registry.mounts() {
            if !self.desired.contains(&mount) {
                let mount_started = Instant::now();
                if self.registry.remove_mount(&mount).is_ok() {
                    info!(
                        mount = mount.as_str(),
                        duration_ms = mount_started.elapsed().as_millis(),
                        "reconcile removed mount"
                    );
                    self.outcome.removed.push(mount.clone());
                }
                self.registry.remove_fingerprint(&mount);
            }
        }
    }
}

/// A planned mount that needs (re)compilation, produced by `plan_path`.
struct LoadWork {
    spec: Spec,
    mount: String,
    wasm_path: PathBuf,
    fingerprint: u64,
    /// Whether a prior instance is being replaced (update) versus a fresh add.
    running: bool,
    reason: &'static str,
}

struct BuiltMount {
    mount: String,
    is_root: bool,
    runtime: Arc<Runtime>,
}

/// The outcome of loading one [`LoadWork`], folded back into the reconcile
/// outcome by `record_load`.
enum LoadResult {
    Ready {
        mount: String,
        wasm_path: PathBuf,
        fingerprint: u64,
        running: bool,
        reason: &'static str,
        duration: Duration,
        built: BuiltMount,
    },
    Failed {
        mount: String,
        reason: String,
    },
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

/// Fingerprint a materialized spec. The spec carries the pinned `ProviderRef`
/// (content id + meta), so the spec hash already captures artifact identity: a
/// swapped-out provider is a new id, hence a new spec hash. No file read or
/// re-hash of the WASM is needed on reconcile.
fn mount_fingerprint(spec: &Spec) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    if let Ok(bytes) = serde_json::to_vec(spec) {
        bytes.hash(&mut hasher);
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
    use omnifs_core::{ProviderId, ProviderMeta, ProviderName};
    use omnifs_mount::materialize::MaterializationMode;
    use omnifs_mount::mounts::Spec;
    use omnifs_provider::ProviderStore;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    /// Lay `src` WASM into the provider store under `providers_dir` and return a
    /// `Spec` (built from `body`, which omits `provider`) pinned to the content
    /// id. Mirrors how the CLI pins a `ProviderRef` after installing an artifact.
    fn pin_spec(providers_dir: &Path, src: &Path, name: &str, mut body: serde_json::Value) -> Spec {
        let bytes = std::fs::read(src).expect("read provider wasm");
        let id = ProviderId::from_wasm_bytes(&bytes);
        let store = ProviderStore::new(providers_dir);
        store.put_if_absent(&id, &bytes).expect("put provider");
        store
            .install(
                id,
                ProviderMeta {
                    name: ProviderName::new(name).unwrap(),
                    version: None,
                },
                format!("{name}.wasm"),
            )
            .expect("install provider");
        body["provider"] = serde_json::json!({ "id": id.to_string(), "meta": { "name": name } });
        serde_json::from_value(body).expect("build pinned spec")
    }

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

    #[tokio::test(flavor = "multi_thread")]
    async fn add_mount_rejects_invalid_provider_config() {
        // The test provider's embedded manifest declares empty config metadata,
        // so the host's validate_instance_config rejects a mount config with
        // extra fields.
        let config_dir = tempfile::tempdir().expect("temp config dir");
        let cache_dir = tempfile::tempdir().expect("temp cache dir");
        let providers_dir = tempfile::tempdir().expect("temp providers dir");
        let paths = omnifs_home::WorkspaceLayout::under_root(config_dir.path());

        let base_wasm = test_provider_wasm_path();
        assert!(
            base_wasm.exists(),
            "test provider missing at {}. Run `just providers build` first.",
            base_wasm.display()
        );

        let cloner = Arc::new(GitCloner::new(cache_dir.path().join("clones")));
        let registry = Arc::new(
            ProviderRegistry::new(
                HostContext::new(
                    cache_dir.path(),
                    &paths.config_dir,
                    providers_dir.path(),
                    &paths.credentials_file,
                ),
                cloner,
            )
            .expect("registry init"),
        );

        // Pin the test provider into the provider store, then mount it with an
        // out-of-schema config field the provider config metadata forbids.
        let spec = pin_spec(
            providers_dir.path(),
            &base_wasm,
            "test-provider",
            serde_json::json!({ "mount": "test", "config": { "unexpected": true } }),
        );

        match registry.add_mount(&spec, &tokio::runtime::Handle::current()) {
            Err(RegistryError::ConfigError(message)) => {
                assert!(message.contains("failed validation"));
                assert!(message.contains("mount test"));
            },
            Err(other) => panic!("expected config error, got {other}"),
            Ok(_) => panic!("expected invalid provider config to be rejected"),
        }
        assert!(registry.mounts().is_empty());
    }

    /// The daemon serve-time backstop: a mount pinning a `ProviderId` whose
    /// artifact is not retained in the provider store is refused at reconcile and
    /// surfaces as a `MountFailure`, never served. Guards "the daemon never
    /// serves a provider it cannot resolve by content id."
    #[tokio::test(flavor = "multi_thread")]
    async fn reconcile_refuses_mount_with_missing_artifact() {
        let config_dir = tempfile::tempdir().expect("temp config dir");
        let cache_dir = tempfile::tempdir().expect("temp cache dir");
        let providers_dir = tempfile::tempdir().expect("temp providers dir");
        let paths = omnifs_home::WorkspaceLayout::under_root(config_dir.path());

        // A mount pinning a content id with no matching retained artifact.
        let mounts_dir = paths.config_dir.join("mounts");
        std::fs::create_dir_all(&mounts_dir).expect("create mounts dir");
        let missing_id = "a".repeat(64);
        std::fs::write(
            mounts_dir.join("db.json"),
            format!(
                r#"{{
                "provider": {{ "id": "{missing_id}", "meta": {{ "name": "db" }} }},
                "mount": "db",
                "config": {{"path": "/data/chinook.sqlite"}}
            }}"#
            ),
        )
        .expect("write spec");

        let cloner = Arc::new(GitCloner::new(cache_dir.path().join("clones")));
        let registry = Arc::new(
            ProviderRegistry::new(
                HostContext::new(
                    cache_dir.path(),
                    &paths.config_dir,
                    providers_dir.path(),
                    &paths.credentials_file,
                ),
                cloner,
            )
            .expect("registry init"),
        );

        let outcome = registry.reconcile(
            &tokio::runtime::Handle::current(),
            MaterializationMode::Docker,
        );

        assert!(
            registry.mounts().is_empty(),
            "a mount with a missing artifact must not be served"
        );
        assert!(outcome.added.is_empty(), "no mount should have been added");
        let failure = outcome
            .failed
            .iter()
            .find(|failure| failure.mount == "db")
            .unwrap_or_else(|| {
                panic!(
                    "expected a failure for mount `db`, got: {:?}",
                    outcome.failed
                )
            });
        assert!(
            failure.reason.contains("provider not found"),
            "failure should report the missing artifact, got: {}",
            failure.reason
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reconcile_keeps_running_mount_when_replacement_fails() {
        let config_dir = tempfile::tempdir().expect("temp config dir");
        let cache_dir = tempfile::tempdir().expect("temp cache dir");
        let providers_dir = tempfile::tempdir().expect("temp providers dir");
        let paths = omnifs_home::WorkspaceLayout::under_root(config_dir.path());

        let base_wasm = test_provider_wasm_path();
        assert!(
            base_wasm.exists(),
            "test provider missing at {}. Run `just providers build` first.",
            base_wasm.display()
        );

        let mounts_dir = paths.config_dir.join("mounts");
        std::fs::create_dir_all(&mounts_dir).expect("create mounts dir");
        let valid_spec = pin_spec(
            providers_dir.path(),
            &base_wasm,
            "test-provider",
            serde_json::json!({
                "mount": "test",
                "config": {},
                "capabilities": { "domains": ["httpbin.org"] }
            }),
        );
        let spec_path = mounts_dir.join("test.json");
        std::fs::write(
            &spec_path,
            serde_json::to_vec_pretty(&valid_spec).expect("serialize valid spec"),
        )
        .expect("write valid spec");

        let cloner = Arc::new(GitCloner::new(cache_dir.path().join("clones")));
        let registry = Arc::new(
            ProviderRegistry::new(
                HostContext::new(
                    cache_dir.path(),
                    &paths.config_dir,
                    providers_dir.path(),
                    &paths.credentials_file,
                ),
                cloner,
            )
            .expect("registry init"),
        );

        let first = registry.reconcile(
            &tokio::runtime::Handle::current(),
            MaterializationMode::Docker,
        );
        assert_eq!(first.added, ["test"], "first reconcile: {first:?}");
        let running = registry.get("test").expect("mount should be running");

        let missing_id = "b".repeat(64);
        std::fs::write(
            &spec_path,
            format!(
                r#"{{
                "provider": {{ "id": "{missing_id}", "meta": {{ "name": "test-provider" }} }},
                "mount": "test",
                "capabilities": {{ "domains": ["httpbin.org"] }},
                "config": {{}}
            }}"#
            ),
        )
        .expect("write broken replacement spec");

        let second = registry.reconcile(
            &tokio::runtime::Handle::current(),
            MaterializationMode::Docker,
        );
        assert!(second.updated.is_empty());
        assert!(second.failed.iter().any(|failure| failure.mount == "test"));
        let still_running = registry
            .get("test")
            .expect("old mount should remain running");
        assert!(Arc::ptr_eq(&running, &still_running));
    }

    /// Shared reconcile setup: a registry over fresh temp dirs plus the mounts
    /// dir a test writes specs into. Holds the temp dirs so they outlive the
    /// registry.
    struct ReconcileFixture {
        registry: Arc<ProviderRegistry>,
        mounts_dir: PathBuf,
        providers_dir: tempfile::TempDir,
        base_wasm: PathBuf,
        _config_dir: tempfile::TempDir,
        _cache_dir: tempfile::TempDir,
    }

    fn reconcile_fixture() -> ReconcileFixture {
        let config_dir = tempfile::tempdir().expect("temp config dir");
        let cache_dir = tempfile::tempdir().expect("temp cache dir");
        let providers_dir = tempfile::tempdir().expect("temp providers dir");
        let paths = omnifs_home::WorkspaceLayout::under_root(config_dir.path());

        let base_wasm = test_provider_wasm_path();
        assert!(
            base_wasm.exists(),
            "test provider missing at {}. Run `just providers build` first.",
            base_wasm.display()
        );

        let mounts_dir = paths.config_dir.join("mounts");
        std::fs::create_dir_all(&mounts_dir).expect("create mounts dir");

        let cloner = Arc::new(GitCloner::new(cache_dir.path().join("clones")));
        let registry = Arc::new(
            ProviderRegistry::new(
                HostContext::new(
                    cache_dir.path(),
                    &paths.config_dir,
                    providers_dir.path(),
                    &paths.credentials_file,
                ),
                cloner,
            )
            .expect("registry init"),
        );

        ReconcileFixture {
            registry,
            mounts_dir,
            providers_dir,
            base_wasm,
            _config_dir: config_dir,
            _cache_dir: cache_dir,
        }
    }

    /// A running mount whose spec changes to a still-valid but different form is
    /// replaced: reconcile reports it as `updated` and swaps in a new runtime
    /// instance (a fresh `Arc`), not the same one.
    #[tokio::test(flavor = "multi_thread")]
    async fn reconcile_updates_running_mount_on_spec_change() {
        let fx = reconcile_fixture();
        let spec = pin_spec(
            fx.providers_dir.path(),
            &fx.base_wasm,
            "test-provider",
            serde_json::json!({
                "mount": "test",
                "config": {},
                "capabilities": { "domains": ["httpbin.org"] }
            }),
        );
        let spec_path = fx.mounts_dir.join("test.json");
        std::fs::write(&spec_path, serde_json::to_vec_pretty(&spec).unwrap()).expect("write spec");

        let handle = tokio::runtime::Handle::current();
        let first = fx.registry.reconcile(&handle, MaterializationMode::Docker);
        assert_eq!(
            first.added,
            ["test"],
            "first reconcile adds the mount: {first:?}"
        );
        let running = fx.registry.get("test").expect("mount should be running");

        // A superset domain grant still satisfies the provider's httpbin.org need
        // but changes the mount fingerprint, so reconcile replaces the instance.
        let mut changed = serde_json::to_value(&spec).unwrap();
        changed["capabilities"]["domains"] = serde_json::json!(["httpbin.org", "example.com"]);
        let changed: Spec = serde_json::from_value(changed).expect("rebuild changed spec");
        std::fs::write(&spec_path, serde_json::to_vec_pretty(&changed).unwrap())
            .expect("write changed spec");

        let second = fx.registry.reconcile(&handle, MaterializationMode::Docker);
        assert_eq!(
            second.updated,
            ["test"],
            "changed spec updates the mount: {second:?}"
        );
        assert!(second.added.is_empty());
        let replaced = fx.registry.get("test").expect("mount running after update");
        assert!(
            !Arc::ptr_eq(&running, &replaced),
            "an update swaps in a new runtime instance"
        );
    }

    /// A mount whose spec file is deleted is removed on the next reconcile:
    /// reported as `removed` and no longer served.
    #[tokio::test(flavor = "multi_thread")]
    async fn reconcile_removes_mount_when_spec_deleted() {
        let fx = reconcile_fixture();
        let spec = pin_spec(
            fx.providers_dir.path(),
            &fx.base_wasm,
            "test-provider",
            serde_json::json!({
                "mount": "test",
                "config": {},
                "capabilities": { "domains": ["httpbin.org"] }
            }),
        );
        let spec_path = fx.mounts_dir.join("test.json");
        std::fs::write(&spec_path, serde_json::to_vec_pretty(&spec).unwrap()).expect("write spec");

        let handle = tokio::runtime::Handle::current();
        let first = fx.registry.reconcile(&handle, MaterializationMode::Docker);
        assert_eq!(first.added, ["test"]);
        assert!(
            fx.registry.get("test").is_some(),
            "mount is running after add"
        );

        std::fs::remove_file(&spec_path).expect("delete spec");
        let second = fx.registry.reconcile(&handle, MaterializationMode::Docker);
        assert_eq!(
            second.removed,
            ["test"],
            "a deleted spec removes the mount: {second:?}"
        );
        assert!(
            fx.registry.get("test").is_none(),
            "a removed mount is no longer served"
        );
        assert!(fx.registry.mounts().is_empty());
    }
}
