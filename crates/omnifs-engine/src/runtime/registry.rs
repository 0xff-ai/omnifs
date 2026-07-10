//! Provider registry: dynamic loading and lifecycle management for WASM providers.
//!
//! Owns the shared engine and caches. Mounts are added and
//! removed at runtime through [`MountRuntimes::add_mount`] and
//! [`MountRuntimes::remove_mount`]; there is no startup directory scan.

use crate::auth::credential_service_for_file;
use crate::cache::Caches;
use crate::cloner::GitCloner;
use crate::snapshot::MountSnapshot;
use crate::{BuildError, HostContext, Runtime, component_engine};
use omnifs_auth::CredentialService;
use omnifs_workspace::mounts::materialize::materialize;
use omnifs_workspace::mounts::{Registry, Spec, UpgradePlan, pinned_manifest};
use omnifs_workspace::provider::Catalog;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tracing::{debug, info, warn};

pub(crate) const DEFAULT_REVALIDATE_SECS: u64 = 15 * 60;

/// Registry of loaded WASM providers.
///
/// Instantiates providers on demand and manages their lifecycle including
/// per-mount timer-driven refresh tasks.
pub struct MountRuntimes {
    engine: wasmtime::Engine,
    caches: Arc<Caches>,
    cloner: Arc<GitCloner>,
    context: HostContext,
    /// The single host-wide credential owner: store access, expiry, and OAuth
    /// refresh for every mount. Shared so mounts resolving to the same
    /// credential share one refresh state.
    credential_service: Arc<CredentialService>,
    instances: parking_lot::RwLock<HashMap<String, Arc<Runtime>>>,
    root_mount: parking_lot::RwLock<Option<String>>,
    timer_shutdown: watch::Sender<bool>,
    timer_tasks: parking_lot::Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
    running_specs: parking_lot::RwLock<HashMap<String, Spec>>,
    /// Per-mount materialized spec fingerprint, used by reconcile to detect a
    /// spec change. The pinned `ProviderRef` is part of the spec, so an artifact
    /// swap shows up as a spec change without re-hashing the WASM.
    fingerprints: parking_lot::RwLock<HashMap<String, u64>>,
    /// Serializes reconcile passes so concurrent triggers cannot race the
    /// add/remove sequence.
    reconcile_lock: parking_lot::Mutex<()>,
}

impl MountRuntimes {
    /// Build an empty registry: engine and cache handles are created once
    /// here and shared across every mount added later.
    pub fn new(context: HostContext, cloner: Arc<GitCloner>) -> Result<Self, RegistryError> {
        // Compiled component artifacts live with the rest of the host's state,
        // under `<cache>/wasm`, rather than a global per-user wasmtime cache.
        let wasm_cache = context.wasm_cache_dir();
        let engine = component_engine(Some(&wasm_cache), |_| {})
            .map_err(|e| RegistryError::RuntimeError(format!("provider engine init: {e}")))?;

        // Global cache handles: a durable object database and a disposable view
        // database cleared + reopened on startup (Codex #5). Shared across all
        // provider runtimes; the object tier isolates mounts by keyspace, the
        // view tier by a path prefix.
        let caches = Caches::open(context.cache_dir())
            .map_err(|e| RegistryError::RuntimeError(format!("cache open: {e}")))?;

        // One credential owner for the whole host, shared across every mount.
        let credential_service = credential_service_for_file(context.credentials_file())
            .map_err(|e| RegistryError::RuntimeError(format!("credential service init: {e}")))?;

        let (timer_shutdown, _) = watch::channel(false);
        Ok(Self {
            engine,
            caches,
            cloner,
            context,
            credential_service,
            instances: parking_lot::RwLock::new(HashMap::new()),
            root_mount: parking_lot::RwLock::new(None),
            timer_shutdown,
            timer_tasks: parking_lot::Mutex::new(HashMap::new()),
            running_specs: parking_lot::RwLock::new(HashMap::new()),
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
        omnifs_workspace::mounts::Name::new(spec.mount.clone())
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
                &self.credential_service,
            )
        } else {
            Runtime::new(
                &self.engine,
                &wasm_path,
                spec,
                self.cloner.clone(),
                &self.context,
                &self.caches,
                &self.credential_service,
            )
        }
        .map_err(|error| registry_error(&mount, error))?;
        Ok(BuiltMount {
            mount,
            is_root,
            revalidate: spec.revalidate,
            fingerprint: mount_fingerprint(spec),
            spec: spec.clone(),
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
            revalidate,
            fingerprint,
            spec,
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
        self.fingerprints.write().insert(mount.clone(), fingerprint);
        self.running_specs.write().insert(mount.clone(), spec);
        self.start_timer(&mount, &runtime, revalidate, handle);
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
            revalidate,
            fingerprint,
            spec,
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

        self.start_timer(&mount, &runtime, revalidate, handle);
        self.fingerprints.write().insert(mount.clone(), fingerprint);
        self.running_specs.write().insert(mount.clone(), spec);
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
        self.fingerprints.write().remove(mount);
        self.running_specs.write().remove(mount);
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

    /// The host-wide credential owner, shared across every mount. The daemon
    /// spawns the proactive OAuth refresh loop on this shared handle.
    pub fn credential_service(&self) -> &Arc<CredentialService> {
        &self.credential_service
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

    /// Build a read-only canonical-store snapshot for a loaded mount.
    pub fn snapshot_mount(&self, mount: &str) -> anyhow::Result<Option<MountSnapshot>> {
        if !self.is_running(mount) {
            return Ok(None);
        }
        MountSnapshot::from_caches(&self.caches, mount).map(Some)
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
    /// canonicalization) and fingerprinted; a spec that is new is added, one
    /// whose fingerprint changed is replaced, one that disappeared is removed,
    /// and one that fails to materialize or instantiate is recorded in
    /// [`ReconcileOutcome::failed`] without aborting the pass.
    pub fn reconcile(self: &Arc<Self>, handle: &tokio::runtime::Handle) -> ReconcileOutcome {
        self.reconcile_with_approvals(handle, UpgradeApprovals::default())
    }

    pub fn reconcile_with_approvals(
        self: &Arc<Self>,
        handle: &tokio::runtime::Handle,
        approvals: UpgradeApprovals,
    ) -> ReconcileOutcome {
        let _guard = self.reconcile_lock.lock();
        ReconcilePass::new(self, handle, approvals, ReconcileScope::all()).run()
    }

    pub fn try_reconcile_scoped(
        self: &Arc<Self>,
        handle: &tokio::runtime::Handle,
        mounts: Option<Vec<String>>,
    ) -> Result<ReconcileOutcome, ReconcileBusy> {
        let Some(_guard) = self.reconcile_lock.try_lock() else {
            return Err(ReconcileBusy);
        };
        Ok(ReconcilePass::new(
            self,
            handle,
            UpgradeApprovals::default(),
            ReconcileScope::from_mounts(mounts),
        )
        .run())
    }

    pub fn converge_spec(
        self: &Arc<Self>,
        handle: &tokio::runtime::Handle,
        spec: Spec,
        approved: Option<UpgradePlan>,
    ) -> ReconcileOutcome {
        let _guard = self.reconcile_lock.lock();
        let mut approvals = UpgradeApprovals::default();
        if let Some(plan) = approved {
            approvals.approve(spec.mount.clone(), plan);
        }
        ReconcilePass::new(self, handle, approvals, ReconcileScope::all()).run_one(spec)
    }

    fn build_work(&self, work: LoadWork) -> LoadResult {
        let LoadWork {
            spec,
            mount,
            wasm_path,
            running,
            reason,
        } = work;
        let started = Instant::now();
        match self.build_mount(&spec, false) {
            Ok(built) => LoadResult::Ready {
                mount,
                wasm_path,
                running,
                reason,
                duration: started.elapsed(),
                built: Box::new(built),
            },
            Err(error) => {
                let kind = error.failure_kind();
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
                    kind,
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

    fn start_timer(
        &self,
        mount: &str,
        runtime: &Arc<Runtime>,
        revalidate: bool,
        handle: &tokio::runtime::Handle,
    ) {
        let provider_interval_secs = runtime.requested_capabilities().refresh_interval_secs;
        if provider_interval_secs == 0 && !revalidate {
            return;
        }

        let interval_secs = if provider_interval_secs == 0 {
            DEFAULT_REVALIDATE_SECS
        } else {
            u64::from(provider_interval_secs)
        };
        let mount = mount.to_string();
        let runtime = Arc::clone(runtime);
        let mut shutdown = self.timer_shutdown.subscribe();
        let task = handle.spawn({
            let mount = mount.clone();
            async move {
                if *shutdown.borrow_and_update() {
                    return;
                }
                let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));
                loop {
                    tokio::select! {
                        _ = interval.tick() => {
                            if provider_interval_secs != 0
                                && let Err(e) = runtime.call_timer_tick().await
                            {
                                debug!(mount = mount.as_str(), error = %e, "provider timer tick failed");
                            }
                            if revalidate {
                                runtime.revalidate_recent_objects().await;
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
    registry: &'a Arc<MountRuntimes>,
    handle: &'a tokio::runtime::Handle,
    providers: Catalog,
    approvals: UpgradeApprovals,
    scope: ReconcileScope,
    desired: HashSet<String>,
    outcome: ReconcileOutcome,
    started: Instant,
}

impl<'a> ReconcilePass<'a> {
    fn new(
        registry: &'a Arc<MountRuntimes>,
        handle: &'a tokio::runtime::Handle,
        approvals: UpgradeApprovals,
        scope: ReconcileScope,
    ) -> Self {
        Self {
            registry,
            handle,
            providers: Catalog::open(registry.context.providers_dir()),
            approvals,
            scope,
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
                    kind: FailureKind::SpecInvalid,
                    reason: format!("scan mounts dir: {error}"),
                    detail: None,
                });
                return self.outcome;
            },
        };

        // A spec that fails to parse or carries an invalid mount name is
        // recorded and skipped; it never aborts the pass or disturbs a running
        // mount.
        for failure in registry
            .failures()
            .iter()
            .filter(|failure| self.scope.includes_failure_path(&failure.path))
        {
            self.outcome.failed.push(MountFailure {
                mount: failure.path.display().to_string(),
                kind: FailureKind::SpecInvalid,
                reason: failure.error.to_string(),
                detail: None,
            });
        }

        // Phase 1 (serial, cheap): materialize, fingerprint, and decide. Settles
        // unchanged mounts and materialize/duplicate failures here; only the
        // compile-heavy loads carry into phase 2.
        let mut work = Vec::new();
        for (name, spec) in registry.iter() {
            if !self.scope.contains(name.as_str()) {
                continue;
            }
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

    fn run_one(mut self, spec: Spec) -> ReconcileOutcome {
        let path = self
            .registry
            .context
            .mounts_dir()
            .join(format!("{}.json", spec.mount));
        if let Some(work) = self.plan_spec(spec, &path) {
            self.record_load(self.registry.build_work(work));
        }
        self.outcome
    }

    /// Decide what a desired spec needs. Records unchanged mounts and
    /// materialize/duplicate failures into the outcome directly; returns a
    /// `LoadWork` only for mounts that must be (re)compiled. `path` is the
    /// spec's on-disk file, used only for failure messages.
    fn plan_spec(&mut self, spec: Spec, path: &Path) -> Option<LoadWork> {
        let materialized = match materialize(spec, &self.providers) {
            Ok(materialized) => materialized,
            Err(error) => {
                self.outcome.failed.push(MountFailure {
                    mount: path.display().to_string(),
                    kind: FailureKind::SpecInvalid,
                    reason: error.to_string(),
                    detail: None,
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

        if running && let Some(failure) = self.consent_failure(&mount, &materialized) {
            self.outcome.failed.push(failure);
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
            running,
            reason,
        })
    }

    fn consent_failure(&self, mount: &str, candidate: &Spec) -> Option<MountFailure> {
        let plan = match self.upgrade_plan(mount, candidate) {
            Ok(plan) => plan,
            Err(error) => {
                return Some(MountFailure {
                    mount: mount.to_string(),
                    kind: error.failure_kind(),
                    reason: error.to_string(),
                    detail: None,
                });
            },
        };
        if !plan.requires_approval() {
            return None;
        }
        if self
            .approvals
            .get(mount)
            .is_some_and(|approved| approved.covers(&plan))
        {
            return None;
        }
        Some(MountFailure {
            mount: mount.to_string(),
            kind: FailureKind::ConsentRequired,
            reason: format!("mount `{mount}` upgrade requires explicit approval"),
            detail: Some(plan),
        })
    }

    fn upgrade_plan(&self, mount: &str, candidate: &Spec) -> Result<UpgradePlan, RegistryError> {
        let running = self
            .registry
            .running_specs
            .read()
            .get(mount)
            .cloned()
            .ok_or_else(|| {
                RegistryError::RuntimeError(format!("running mount `{mount}` has no recorded spec"))
            })?;
        let old = pinned_manifest(&self.providers, &running)
            .map_err(|error| RegistryError::ConfigError(error.to_string()))?
            .ok_or_else(|| {
                RegistryError::ProviderNotFound(format!(
                    "running provider artifact for mount `{mount}` is missing"
                ))
            })?;
        let new = pinned_manifest(&self.providers, candidate)
            .map_err(|error| RegistryError::ConfigError(error.to_string()))?
            .ok_or_else(|| {
                RegistryError::ProviderNotFound(format!(
                    "candidate provider artifact for mount `{mount}` is missing"
                ))
            })?;
        Ok(UpgradePlan::diff(&old, &new))
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
                    kind: FailureKind::Internal,
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
                running,
                reason,
                duration,
                built,
            } => {
                let apply_started = Instant::now();
                let applied = if running {
                    self.registry.replace_mount(*built, self.handle)
                } else {
                    self.registry
                        .publish_new_mount(*built, self.handle)
                        .map(|_| ())
                };
                match applied {
                    Ok(()) => {
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
                            kind: error.failure_kind(),
                            reason: error.to_string(),
                            detail: None,
                        });
                    },
                }
            },
            LoadResult::Failed {
                mount,
                kind,
                reason,
            } => {
                self.outcome.failed.push(MountFailure {
                    mount,
                    kind,
                    reason,
                    detail: None,
                });
            },
        }
    }

    fn remove_stale_mounts(&mut self) {
        for mount in self.registry.mounts() {
            if self.scope.contains(&mount) && !self.desired.contains(&mount) {
                let mount_started = Instant::now();
                if self.registry.remove_mount(&mount).is_ok() {
                    info!(
                        mount = mount.as_str(),
                        duration_ms = mount_started.elapsed().as_millis(),
                        "reconcile removed mount"
                    );
                    self.outcome.removed.push(mount.clone());
                }
            }
        }
    }
}

struct ReconcileScope {
    mounts: Option<HashSet<String>>,
}

impl ReconcileScope {
    fn all() -> Self {
        Self { mounts: None }
    }

    fn from_mounts(mounts: Option<Vec<String>>) -> Self {
        Self {
            mounts: mounts.map(|mounts| mounts.into_iter().collect()),
        }
    }

    fn contains(&self, mount: &str) -> bool {
        match &self.mounts {
            Some(mounts) => mounts.contains(mount),
            None => true,
        }
    }

    fn includes_failure_path(&self, path: &Path) -> bool {
        match &self.mounts {
            Some(mounts) => path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .is_some_and(|stem| mounts.contains(stem)),
            None => true,
        }
    }
}

/// A planned mount that needs (re)compilation, produced by `plan_path`.
struct LoadWork {
    spec: Spec,
    mount: String,
    wasm_path: PathBuf,
    /// Whether a prior instance is being replaced (update) versus a fresh add.
    running: bool,
    reason: &'static str,
}

struct BuiltMount {
    mount: String,
    is_root: bool,
    revalidate: bool,
    fingerprint: u64,
    spec: Spec,
    runtime: Arc<Runtime>,
}

/// The outcome of loading one [`LoadWork`], folded back into the reconcile
/// outcome by `record_load`.
enum LoadResult {
    Ready {
        mount: String,
        wasm_path: PathBuf,
        running: bool,
        reason: &'static str,
        duration: Duration,
        built: Box<BuiltMount>,
    },
    Failed {
        mount: String,
        kind: FailureKind,
        reason: String,
    },
}

/// One mount that did not converge during [`MountRuntimes::reconcile`].
#[derive(Debug, Clone)]
pub struct MountFailure {
    /// Mount name, or the spec path when the name could not be parsed.
    pub mount: String,
    pub kind: FailureKind,
    pub reason: String,
    pub detail: Option<UpgradePlan>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    ConsentRequired,
    SpecInvalid,
    ProviderMissing,
    Internal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconcileBusy;

/// What a reconcile pass changed. Host-local; the daemon maps it to the
/// control-API report type.
#[derive(Debug, Default)]
pub struct ReconcileOutcome {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub updated: Vec<String>,
    pub failed: Vec<MountFailure>,
}

#[derive(Debug, Clone, Default)]
pub struct UpgradeApprovals {
    plans: HashMap<String, UpgradePlan>,
}

impl UpgradeApprovals {
    pub fn approve(&mut self, mount: impl Into<String>, plan: UpgradePlan) {
        self.plans.insert(mount.into(), plan);
    }

    #[must_use]
    pub fn get(&self, mount: &str) -> Option<&UpgradePlan> {
        self.plans.get(mount)
    }
}

/// Fingerprint a materialized spec. The spec carries the pinned `ProviderRef`
/// (content id + meta), so the spec hash already captures artifact identity: a
/// swapped-out provider is a new id, hence a new spec hash. No file read or
/// re-hash of the WASM is needed on reconcile.
fn mount_fingerprint(spec: &Spec) -> u64 {
    let bytes = serde_json::to_vec(spec).unwrap_or_default();
    let digest = blake3::hash(&bytes);
    let prefix = digest.as_bytes()[..8]
        .try_into()
        .expect("blake3 digest is at least 8 bytes");
    u64::from_le_bytes(prefix)
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

impl RegistryError {
    fn failure_kind(&self) -> FailureKind {
        match self {
            Self::ConfigError(_) | Self::DuplicateMount(_) | Self::MountNotFound(_) => {
                FailureKind::SpecInvalid
            },
            Self::ProviderNotFound(_) => FailureKind::ProviderMissing,
            Self::RuntimeError(_) => FailureKind::Internal,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{FailureKind, MountRuntimes, RegistryError};
    use crate::HostContext;
    use crate::Runtime;
    use crate::cache::Caches;
    use crate::cloner::GitCloner;
    use crate::ops::namespace::ReadBytes;
    use crate::test_support::PendingTestCallout;
    use omnifs_core::path::Path as OmnifsPath;
    use omnifs_wit::provider::types::{Callout, CalloutResult, Header, HttpResponse};
    use omnifs_workspace::ids::{ProviderId, ProviderMeta, ProviderName};
    use omnifs_workspace::mounts::{Spec, UpgradePlan};
    use omnifs_workspace::provider::ProviderStore;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Duration;

    /// Lay `src` WASM into the provider store under `providers_dir` and return a
    /// `Spec` (built from `body`, which omits `provider`) pinned to the content
    /// id. Mirrors how the CLI pins a `ProviderRef` after installing an artifact.
    fn pin_spec(providers_dir: &Path, src: &Path, name: &str, body: serde_json::Value) -> Spec {
        let bytes = std::fs::read(src).expect("read provider wasm");
        pin_spec_bytes(providers_dir, &bytes, name, body)
    }

    fn pin_spec_bytes(
        providers_dir: &Path,
        bytes: &[u8],
        name: &str,
        mut body: serde_json::Value,
    ) -> Spec {
        let id = ProviderId::from_wasm_bytes(bytes);
        let store = ProviderStore::new(providers_dir);
        store.put_if_absent(&id, bytes).expect("put provider");
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

    fn provider_wasm_with_capabilities(src: &Path, capabilities: &serde_json::Value) -> Vec<u8> {
        let base = std::fs::read(src).expect("read provider wasm");
        let metadata = serde_json::json!({
            "id": "test-provider",
            "displayName": "Test Provider",
            "provider": "test_provider.wasm",
            "defaultMount": "test",
            "capabilities": capabilities,
        });
        omnifs_workspace::provider::embed_provider_metadata_section(
            &base,
            serde_json::to_vec(&metadata).unwrap().as_slice(),
        )
        .expect("embed provider metadata")
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

    fn remote_item_body(title: &str) -> Vec<u8> {
        format!(r#"{{"number":9,"title":"{title}","body":"Body 9","state":"open"}}"#).into_bytes()
    }

    fn http_response(status: u16, etag: Option<&str>, body: Vec<u8>) -> CalloutResult {
        let mut headers = Vec::new();
        if let Some(etag) = etag {
            headers.push(Header {
                name: "etag".to_string(),
                value: etag.to_string(),
            });
        }
        CalloutResult::HttpResponse(HttpResponse {
            status,
            headers,
            body,
        })
    }

    async fn next_callout(runtime: &Runtime) -> PendingTestCallout {
        // Wall-clock deadline, not a yield-count budget: the callout lands only
        // after real wasm work whose poll count varies wildly with machine
        // load, and tokio test time is paused here so timers cannot be used.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while std::time::Instant::now() < deadline {
            if let Some(callout) = runtime.try_recv_test_callout() {
                return callout;
            }
            tokio::task::yield_now().await;
        }
        panic!("provider did not issue a test callout");
    }

    fn fetch_header<'a>(callout: &'a PendingTestCallout, name: &str) -> Option<&'a str> {
        let Callout::Fetch(request) = callout.callout() else {
            panic!("expected fetch callout, got {:?}", callout.callout());
        };
        request
            .headers
            .iter()
            .find(|header| header.name.eq_ignore_ascii_case(name))
            .map(|header| header.value.as_str())
    }

    async fn wait_for_cached_bytes(runtime: &Runtime, path: &OmnifsPath, expected: &[u8]) {
        // Wall-clock deadline, not a yield-count budget: the refresh lands only
        // after real wasm work whose poll count varies wildly with machine load
        // (a cold wasmtime compile cache flaked a fixed 100-iteration budget).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while std::time::Instant::now() < deadline {
            if runtime
                .cache()
                .cached_canonical_for(path)
                .is_some_and(|canonical| canonical.bytes == expected)
            {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("canonical cache did not refresh to expected bytes");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn add_mount_rejects_invalid_provider_config() {
        // The test provider's embedded manifest declares empty config metadata,
        // so the host's validate_instance_config rejects a mount config with
        // extra fields.
        let config_dir = tempfile::tempdir().expect("temp config dir");
        let cache_dir = tempfile::tempdir().expect("temp cache dir");
        let providers_dir = tempfile::tempdir().expect("temp providers dir");
        let paths = omnifs_workspace::layout::WorkspaceLayout::under_root(config_dir.path());

        let base_wasm = test_provider_wasm_path();
        assert!(
            base_wasm.exists(),
            "test provider missing at {}. Run `just providers build` first.",
            base_wasm.display()
        );

        let cloner = Arc::new(GitCloner::new(cache_dir.path().join("clones")));
        let registry = Arc::new(
            MountRuntimes::new(
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
        let paths = omnifs_workspace::layout::WorkspaceLayout::under_root(config_dir.path());

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
            MountRuntimes::new(
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

        let outcome = registry.reconcile(&tokio::runtime::Handle::current());

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
        let paths = omnifs_workspace::layout::WorkspaceLayout::under_root(config_dir.path());

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
            MountRuntimes::new(
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

        let first = registry.reconcile(&tokio::runtime::Handle::current());
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

        let second = registry.reconcile(&tokio::runtime::Handle::current());
        assert!(second.updated.is_empty());
        assert!(second.failed.iter().any(|failure| failure.mount == "test"));
        let still_running = registry
            .get("test")
            .expect("old mount should remain running");
        assert!(Arc::ptr_eq(&running, &still_running));
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    #[allow(clippy::too_many_lines)]
    async fn revalidation_timer_refreshes_recent_object_without_provider_invalidation() {
        let config_dir = tempfile::tempdir().expect("temp config dir");
        let cache_dir = tempfile::tempdir().expect("temp cache dir");
        let providers_dir = tempfile::tempdir().expect("temp providers dir");
        let paths = omnifs_workspace::layout::WorkspaceLayout::under_root(config_dir.path());

        let base_wasm = test_provider_wasm_path();
        assert!(
            base_wasm.exists(),
            "test provider missing at {}. Run `just providers build` first.",
            base_wasm.display()
        );
        let spec = pin_spec(
            providers_dir.path(),
            &base_wasm,
            "test-provider",
            serde_json::json!({
                "mount": "test",
                "config": {},
                "capabilities": { "domains": ["httpbin.org"] }
            }),
        );

        let engine = crate::component_engine(None, |_| {}).expect("engine");
        let cloner = Arc::new(GitCloner::new(cache_dir.path().join("clones")));
        let caches = Caches::open(cache_dir.path()).expect("cache open");
        let context = HostContext::new(
            cache_dir.path(),
            &paths.config_dir,
            providers_dir.path(),
            &paths.credentials_file,
        );
        let credential_service = crate::auth::credential_service_for_file(&paths.credentials_file)
            .expect("credential service");
        let runtime = Arc::new(
            Runtime::new_for_callout_tests(
                &engine,
                &base_wasm,
                &spec,
                cloner,
                &context,
                &caches,
                &credential_service,
            )
            .expect("runtime"),
        );

        let timer_config_dir = tempfile::tempdir().expect("timer config dir");
        let timer_cache_dir = tempfile::tempdir().expect("timer cache dir");
        let timer_providers_dir = tempfile::tempdir().expect("timer providers dir");
        let timer_paths =
            omnifs_workspace::layout::WorkspaceLayout::under_root(timer_config_dir.path());
        let timer_registry = MountRuntimes::new(
            HostContext::new(
                timer_cache_dir.path(),
                &timer_paths.config_dir,
                timer_providers_dir.path(),
                &timer_paths.credentials_file,
            ),
            Arc::new(GitCloner::new(timer_cache_dir.path().join("clones"))),
        )
        .expect("timer registry");

        timer_registry.start_timer(
            "test",
            &runtime,
            spec.revalidate,
            &tokio::runtime::Handle::current(),
        );
        tokio::task::yield_now().await;

        let path = OmnifsPath::parse("/items/open/9/item.json").unwrap();
        let content_type = path.content_type_mime(None).to_string();
        let stale = remote_item_body("stale");
        let fresh = remote_item_body("fresh");

        let namespace = runtime.namespace();
        let read = namespace.read_file(&path, content_type.clone(), None);
        let answer = async {
            let callout = next_callout(&runtime).await;
            assert_eq!(fetch_header(&callout, "if-none-match"), None);
            callout.answer(http_response(200, Some("item-9-v1"), stale.clone()));
        };
        let (read_result, ()) = tokio::join!(read, answer);
        read_result.expect("initial read");
        wait_for_cached_bytes(&runtime, &path, &stale).await;
        let _ = runtime.drain_invalidated_paths();

        tokio::time::advance(Duration::from_mins(1)).await;
        let callout = next_callout(&runtime).await;
        assert_eq!(fetch_header(&callout, "if-none-match"), Some("item-9-v1"));
        callout.answer(http_response(200, Some("item-9-v2"), fresh.clone()));
        wait_for_cached_bytes(&runtime, &path, &fresh).await;

        let subsequent = runtime
            .namespace()
            .read_file(&path, content_type, None)
            .await
            .expect("subsequent read");
        assert!(matches!(subsequent.bytes, ReadBytes::Canonical));
        assert_eq!(
            runtime.canonical_bytes_for(&path).as_deref(),
            Some(fresh.as_slice())
        );
        assert!(
            !runtime
                .drain_invalidated_paths()
                .iter()
                .any(|invalidated| invalidated == &path),
            "item 9 must refresh without an explicit provider invalidation"
        );

        timer_registry.shutdown_all();
        runtime.shutdown().expect("runtime shutdown");
    }

    /// Shared reconcile setup: a registry over fresh temp dirs plus the mounts
    /// dir a test writes specs into. Holds the temp dirs so they outlive the
    /// registry.
    struct ReconcileFixture {
        registry: Arc<MountRuntimes>,
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
        let paths = omnifs_workspace::layout::WorkspaceLayout::under_root(config_dir.path());

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
            MountRuntimes::new(
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
        let first = fx.registry.reconcile(&handle);
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

        let second = fx.registry.reconcile(&handle);
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

    #[tokio::test(flavor = "multi_thread")]
    async fn reconcile_requires_approval_for_capability_widening() {
        let fx = reconcile_fixture();
        let old_wasm = provider_wasm_with_capabilities(
            &fx.base_wasm,
            &serde_json::json!([
                { "kind": "domain", "value": "httpbin.org", "why": "test api" }
            ]),
        );
        let new_wasm = provider_wasm_with_capabilities(
            &fx.base_wasm,
            &serde_json::json!([
                { "kind": "domain", "value": "httpbin.org", "why": "test api" },
                { "kind": "domain", "value": "example.com", "why": "new api" }
            ]),
        );
        let spec = pin_spec_bytes(
            fx.providers_dir.path(),
            &old_wasm,
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
        let first = fx.registry.reconcile(&handle);
        assert_eq!(first.added, ["test"], "first reconcile: {first:?}");
        let running = fx.registry.get("test").expect("mount should be running");

        let widened = pin_spec_bytes(
            fx.providers_dir.path(),
            &new_wasm,
            "test-provider",
            serde_json::json!({
                "mount": "test",
                "config": {},
                "capabilities": { "domains": ["httpbin.org", "example.com"] }
            }),
        );
        std::fs::write(&spec_path, serde_json::to_vec_pretty(&widened).unwrap())
            .expect("write widened spec");

        let refused = fx.registry.reconcile(&handle);
        assert!(
            refused.updated.is_empty(),
            "unapproved widening must not update: {refused:?}"
        );
        let failure = refused
            .failed
            .iter()
            .find(|failure| failure.mount == "test")
            .expect("widening failure");
        assert_eq!(failure.kind, FailureKind::ConsentRequired);
        let actual = failure.detail.clone().expect("actual approval delta");
        assert!(matches!(actual, UpgradePlan::CapabilityLimitOrAuth { .. }));
        let still_running = fx.registry.get("test").expect("old mount remains");
        assert!(Arc::ptr_eq(&running, &still_running));

        let mut approvals = super::UpgradeApprovals::default();
        approvals.approve("test", actual);
        let approved = fx.registry.reconcile_with_approvals(&handle, approvals);
        assert_eq!(
            approved.updated,
            ["test"],
            "approved widening updates the mount: {approved:?}"
        );
        let replaced = fx.registry.get("test").expect("mount running after update");
        assert!(
            !Arc::ptr_eq(&running, &replaced),
            "approval permits the runtime swap"
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
        let first = fx.registry.reconcile(&handle);
        assert_eq!(first.added, ["test"]);
        assert!(
            fx.registry.get("test").is_some(),
            "mount is running after add"
        );

        std::fs::remove_file(&spec_path).expect("delete spec");
        let second = fx.registry.reconcile(&handle);
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

    #[tokio::test(flavor = "multi_thread")]
    async fn scoped_reconcile_leaves_out_of_scope_mount_untouched() {
        let fx = reconcile_fixture();
        let test_spec = pin_spec(
            fx.providers_dir.path(),
            &fx.base_wasm,
            "test-provider",
            serde_json::json!({
                "mount": "test",
                "config": {},
                "capabilities": { "domains": ["httpbin.org"] }
            }),
        );
        let other_spec = pin_spec(
            fx.providers_dir.path(),
            &fx.base_wasm,
            "test-provider",
            serde_json::json!({
                "mount": "other",
                "config": {},
                "capabilities": { "domains": ["httpbin.org"] }
            }),
        );
        let test_path = fx.mounts_dir.join("test.json");
        let other_path = fx.mounts_dir.join("other.json");
        std::fs::write(&test_path, serde_json::to_vec_pretty(&test_spec).unwrap())
            .expect("write test spec");
        std::fs::write(&other_path, serde_json::to_vec_pretty(&other_spec).unwrap())
            .expect("write other spec");

        let handle = tokio::runtime::Handle::current();
        let first = fx.registry.reconcile(&handle);
        assert_eq!(
            first.added,
            ["other", "test"],
            "first reconcile adds both mounts: {first:?}"
        );
        let other_running = fx.registry.get("other").expect("other mount is running");

        std::fs::remove_file(&other_path).expect("delete out-of-scope spec");
        let scoped = fx
            .registry
            .try_reconcile_scoped(&handle, Some(vec!["test".to_string()]))
            .expect("scoped reconcile must acquire lock");

        assert!(
            scoped.removed.is_empty(),
            "out-of-scope deleted spec must not remove a running mount: {scoped:?}"
        );
        let still_running = fx.registry.get("other").expect("other mount still running");
        assert!(Arc::ptr_eq(&other_running, &still_running));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn try_reconcile_scoped_reports_busy_when_lock_is_held() {
        let config_dir = tempfile::tempdir().expect("temp config dir");
        let cache_dir = tempfile::tempdir().expect("temp cache dir");
        let providers_dir = tempfile::tempdir().expect("temp providers dir");
        let paths = omnifs_workspace::layout::WorkspaceLayout::under_root(config_dir.path());
        let registry = Arc::new(
            MountRuntimes::new(
                HostContext::new(
                    cache_dir.path(),
                    &paths.config_dir,
                    providers_dir.path(),
                    &paths.credentials_file,
                ),
                Arc::new(GitCloner::new(cache_dir.path().join("clones"))),
            )
            .expect("registry init"),
        );
        let handle = tokio::runtime::Handle::current();
        let _guard = registry.reconcile_lock.lock();

        let result = registry.try_reconcile_scoped(&handle, None);

        assert!(matches!(result, Err(super::ReconcileBusy)));
    }
}
