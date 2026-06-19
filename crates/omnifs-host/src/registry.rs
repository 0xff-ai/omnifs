//! Provider registry: dynamic loading and lifecycle management for WASM providers.
//!
//! Owns the shared engine, extractor, and caches. Mounts are added and
//! removed at runtime through [`ProviderRegistry::add_mount`] and
//! [`ProviderRegistry::remove_mount`]; there is no startup directory scan.

use crate::cloner::GitCloner;
use crate::tools::archive::{ARCHIVE_TOOL_WASM, ArchiveExtractorComponent, DEFAULT_LIMITS};
use crate::{Artifact, BuildError, Dirs, Runtime, component_engine};
use omnifs_cache::Caches;
use omnifs_mount::materialize::materialize;
use omnifs_mount::mounts::{Catalog, Resolved, Spec, spec_paths_in};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
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
    cache_dir: PathBuf,
    config_dir: PathBuf,
    providers_dir: PathBuf,
    credentials_file: PathBuf,
    instances: parking_lot::RwLock<HashMap<String, Arc<Runtime>>>,
    root_mount: parking_lot::RwLock<Option<String>>,
    timer_shutdown: watch::Sender<bool>,
    timer_tasks: parking_lot::Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
    /// Per-mount materialized fingerprint, used by [`Self::reconcile`] to detect
    /// a spec or provider-artifact change. Keyed by mount name.
    fingerprints: parking_lot::RwLock<HashMap<String, u64>>,
    /// Serializes reconcile passes so concurrent triggers cannot race the
    /// add/remove sequence.
    reconcile_lock: parking_lot::Mutex<()>,
}

impl ProviderRegistry {
    /// Build an empty registry: engine, archive extractor, and cache handles
    /// are created once here and shared across every mount added later.
    pub fn new(dirs: Dirs<'_>, cloner: Arc<GitCloner>) -> Result<Self, RegistryError> {
        let engine = component_engine(|_| {})
            .map_err(|e| RegistryError::RuntimeError(format!("provider engine init: {e}")))?;

        // One extractor (engine + parsed component + linker pre) shared
        // across every mount; the per-call sandbox lives on a fresh
        // `wasmtime::Store`.
        let archive_tool_path = dirs.provider_path(ARCHIVE_TOOL_WASM);
        let extractor = Arc::new(
            ArchiveExtractorComponent::from_path(&archive_tool_path, DEFAULT_LIMITS)
                .map_err(|e| RegistryError::RuntimeError(format!("extractor init: {e}")))?,
        );

        // Global cache handles: a durable object database and a disposable view
        // database cleared + reopened on startup (Codex #5). Shared across all
        // provider runtimes; the object tier isolates mounts by keyspace, the
        // view tier by a path prefix.
        let caches = Caches::open(dirs.cache_dir)
            .map_err(|e| RegistryError::RuntimeError(format!("cache open: {e}")))?;

        let (timer_shutdown, _) = watch::channel(false);
        Ok(Self {
            engine,
            extractor,
            caches,
            cloner,
            cache_dir: dirs.cache_dir.to_path_buf(),
            config_dir: dirs.config_dir.to_path_buf(),
            providers_dir: dirs.providers_dir.to_path_buf(),
            credentials_file: dirs.credentials_file.to_path_buf(),
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
        spec: Spec,
        handle: &tokio::runtime::Handle,
    ) -> Result<Arc<Runtime>, RegistryError> {
        omnifs_core::mount::Name::new(spec.mount.clone())
            .map_err(|error| RegistryError::ConfigError(format!("invalid mount name: {error}")))?;
        let mount = spec.mount.clone();
        if self.instances.read().contains_key(&mount) {
            return Err(RegistryError::DuplicateMount(mount));
        }

        let dirs = Dirs::new(
            &self.cache_dir,
            &self.config_dir,
            &self.providers_dir,
            &self.credentials_file,
        );
        let wasm_path = dirs.provider_path(&spec.provider);
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
            &self.engine,
            &wasm_path,
            &resolved,
            self.cloner.clone(),
            dirs,
            self.extractor.clone(),
            &self.caches,
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

    /// The directories this registry resolves mounts against.
    pub fn dirs(&self) -> Dirs<'_> {
        Dirs::new(
            &self.cache_dir,
            &self.config_dir,
            &self.providers_dir,
            &self.credentials_file,
        )
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
    /// [`ReconcileOutcome::failed`] without aborting the pass. `host_native`
    /// selects host-direct preopens (true) versus container-rewritten preopens.
    pub fn reconcile(
        &self,
        handle: &tokio::runtime::Handle,
        host_native: bool,
    ) -> ReconcileOutcome {
        let _guard = self.reconcile_lock.lock();
        let mut outcome = ReconcileOutcome::default();

        let catalog = Catalog::new(self.config_dir.join("mounts"), &self.providers_dir);
        let paths = match spec_paths_in(catalog.mounts_dir()) {
            Ok(paths) => paths,
            Err(error) => {
                outcome.failed.push(MountFailure {
                    mount: catalog.mounts_dir().display().to_string(),
                    reason: format!("scan mounts dir: {error}"),
                });
                return outcome;
            },
        };

        let mut desired = HashSet::new();
        for path in paths {
            let spec = match Spec::from_file(&path) {
                Ok(spec) => spec,
                Err(error) => {
                    outcome.failed.push(MountFailure {
                        mount: path.display().to_string(),
                        reason: error.to_string(),
                    });
                    continue;
                },
            };
            let materialized = match materialize(spec, &catalog, host_native) {
                Ok(materialized) => materialized.spec,
                Err(error) => {
                    outcome.failed.push(MountFailure {
                        mount: path.display().to_string(),
                        reason: error.to_string(),
                    });
                    continue;
                },
            };
            let mount = materialized.mount.clone();
            desired.insert(mount.clone());

            let wasm_path = self.dirs().provider_path(&materialized.provider);
            let fingerprint = mount_fingerprint(&materialized, &wasm_path);
            let running = self.instances.read().contains_key(&mount);
            let unchanged = running && self.fingerprints.read().get(&mount) == Some(&fingerprint);
            if unchanged {
                continue;
            }
            if running {
                let _ = self.remove_mount(&mount);
            }
            match self.add_mount(materialized, handle) {
                Ok(_) => {
                    self.fingerprints.write().insert(mount.clone(), fingerprint);
                    if running {
                        outcome.updated.push(mount);
                    } else {
                        outcome.added.push(mount);
                    }
                },
                Err(error) => {
                    self.fingerprints.write().remove(&mount);
                    outcome.failed.push(MountFailure {
                        mount,
                        reason: error.to_string(),
                    });
                },
            }
        }

        for mount in self.mounts() {
            if !desired.contains(&mount) {
                if self.remove_mount(&mount).is_ok() {
                    outcome.removed.push(mount.clone());
                }
                self.fingerprints.write().remove(&mount);
            }
        }

        outcome
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

/// Fingerprint a materialized spec plus its provider artifact, so a reconcile
/// detects both config edits and a swapped-out provider binary. The provider
/// stamp uses file length and mtime rather than a content hash to keep the pass
/// cheap; a rebuilt provider changes both.
fn mount_fingerprint(spec: &Spec, wasm_path: &Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    if let Ok(bytes) = serde_json::to_vec(spec) {
        bytes.hash(&mut hasher);
    }
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
    use crate::Dirs;
    use crate::cloner::GitCloner;
    use crate::tools::archive::ARCHIVE_TOOL_WASM;
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
        let paths = omnifs_home::Paths::under_root(config_dir.path());

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
            Dirs::new(
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
}
