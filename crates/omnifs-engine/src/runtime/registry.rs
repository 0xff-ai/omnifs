//! Provider registry: startup loading and lifecycle management for WASM providers.
//!
//! Startup is atomic: the complete immutable mount snapshot is scanned,
//! materialized, and instantiated before any runtime is published.

use crate::auth::credential_service_for_file;
use crate::cache::Caches;
use crate::cloner::GitCloner;
use crate::snapshot::MountSnapshot;
use crate::{BuildError, HostContext, Runtime, component_engine};
use omnifs_auth::CredentialService;
use omnifs_workspace::mounts::materialize::materialize;
use omnifs_workspace::mounts::{Registry, Spec};
use omnifs_workspace::provider::Catalog;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
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
    timer_shutdown: watch::Sender<bool>,
    timer_tasks: parking_lot::Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
}

impl MountRuntimes {
    /// Build an empty registry: engine and cache handles are created once
    /// here and shared across every mount added later.
    pub fn new(context: HostContext, cloner: Arc<GitCloner>) -> Result<Self, RegistryError> {
        // Compiled component artifacts live with the rest of the host's state,
        // under `<cache>/wasm`, rather than a global per-user wasmtime cache.
        let wasm_cache = context.wasm_cache_dir();
        let engine = component_engine(Some(wasm_cache), |_| {})
            .map_err(|e| RegistryError::RuntimeError(format!("provider engine init: {e}")))?;

        // Global cache handles: a durable object database and a disposable view
        // database cleared and reopened on startup. Shared across all
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
            timer_shutdown,
            timer_tasks: parking_lot::Mutex::new(HashMap::new()),
        })
    }

    /// Load and publish every mount from the immutable snapshot selected by the
    /// host context. Any scan, materialization, artifact, duplicate, or runtime
    /// error aborts startup before the first runtime is visible.
    pub fn load(
        context: HostContext,
        cloner: Arc<GitCloner>,
        desired: &Registry,
        handle: &tokio::runtime::Handle,
    ) -> Result<Self, RegistryError> {
        let registry = Self::new(context, cloner)?;
        if let Some(failure) = desired.failures().first() {
            return Err(RegistryError::ConfigError(format!(
                "load mount spec {}: {}",
                failure.path.display(),
                failure.error
            )));
        }
        let catalog = Catalog::open(registry.context.providers_dir());
        let built = desired
            .iter()
            .map(|(_, spec)| {
                let spec = materialize(spec.clone(), &catalog)
                    .map_err(|error| RegistryError::ConfigError(error.to_string()))?;
                registry.build_mount(&spec, false)
            })
            .collect::<Result<Vec<_>, _>>()?;
        for mount in built {
            registry.publish_new_mount(mount, handle)?;
        }
        Ok(registry)
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
        .map_err(|error| RegistryError::from_build(&mount, error))?;
        Ok(BuiltMount {
            mount,
            revalidate: spec.revalidate,
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
            revalidate,
            runtime,
        } = built;
        {
            let mut instances = self.instances.write();
            if instances.contains_key(&mount) {
                return Err(RegistryError::DuplicateMount(mount));
            }
            instances.insert(mount.clone(), Arc::clone(&runtime));
        }
        self.start_timer(&mount, &runtime, revalidate, handle);
        info!(mount = mount.as_str(), "loaded provider");
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

    fn is_running(&self, mount: &str) -> bool {
        self.instances.read().contains_key(mount)
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

struct BuiltMount {
    mount: String,
    revalidate: bool,
    runtime: Arc<Runtime>,
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
    fn from_build(mount: &str, error: BuildError) -> Self {
        match error {
            BuildError::InvalidConfig(message) => {
                Self::ConfigError(format!("mount {mount}: {message}"))
            },
            other => Self::RuntimeError(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{MountRuntimes, RegistryError};
    use crate::HostContext;
    use crate::cloner::GitCloner;
    use omnifs_workspace::ids::{ProviderId, ProviderMeta, ProviderName};
    use omnifs_workspace::mounts::{Registry, Spec};
    use omnifs_workspace::provider::ProviderStore;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

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
        let paths = omnifs_workspace::layout::WorkspaceLayout::under_root(config_dir.path());

        let base_wasm = test_provider_wasm_path();
        assert!(
            base_wasm.exists(),
            "test provider missing at {}. Run `just build providers` first.",
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
                )
                .with_wasm_cache_dir(crate::test_support::wasm_cache_dir()),
                cloner,
            )
            .expect("registry init"),
        );

        // Pin the test provider into the provider store, then mount it with an
        // out-of-schema config field and an invalid literal preopen. Config
        // validation must win over preopen validation during runtime build.
        let spec = pin_spec(
            providers_dir.path(),
            &base_wasm,
            "test-provider",
            serde_json::json!({
                "mount": "test",
                "config": { "unexpected": true },
                "capabilities": {
                    "preopened_paths": [{
                        "host": "relative",
                        "guest": "/data",
                        "mode": "ro"
                    }]
                }
            }),
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

    #[tokio::test(flavor = "multi_thread")]
    async fn load_rejects_missing_provider_artifact() {
        let root = tempfile::tempdir().expect("temp root");
        let cache = root.path().join("cache");
        let config = root.path().join("config");
        let mounts = root.path().join("snapshot");
        let providers = root.path().join("providers");
        std::fs::create_dir_all(&mounts).expect("mounts");
        std::fs::create_dir_all(&providers).expect("providers");
        std::fs::write(
            mounts.join("missing.json"),
            format!(
                r#"{{
            "provider": {{ "id": "{}", "meta": {{ "name": "missing" }} }},
            "mount": "missing"
        }}"#,
                "a".repeat(64)
            ),
        )
        .expect("spec");
        let context =
            HostContext::new(&cache, &config, &providers, root.path().join("credentials"))
                .with_wasm_cache_dir(crate::test_support::wasm_cache_dir());
        let result = MountRuntimes::load(
            context,
            Arc::new(GitCloner::new(root.path().join("clones"))),
            &Registry::load(&mounts).expect("load selected snapshot"),
            &tokio::runtime::Handle::current(),
        );
        assert!(matches!(result, Err(RegistryError::ProviderNotFound(_))));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn load_rejects_malformed_snapshot() {
        let root = tempfile::tempdir().expect("temp root");
        let mounts = root.path().join("snapshot");
        std::fs::create_dir_all(&mounts).expect("mounts");
        std::fs::write(mounts.join("broken.json"), b"not json").expect("spec");
        let context = HostContext::new(
            root.path().join("cache"),
            root.path().join("config"),
            root.path().join("providers"),
            root.path().join("credentials"),
        );
        let result = MountRuntimes::load(
            context,
            Arc::new(GitCloner::new(root.path().join("clones"))),
            &Registry::load(&mounts).expect("load selected snapshot"),
            &tokio::runtime::Handle::current(),
        );
        assert!(
            matches!(result, Err(RegistryError::ConfigError(message)) if message.contains("broken.json"))
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn load_publishes_all_snapshot_mounts() {
        let root = tempfile::tempdir().expect("temp root");
        let mounts = root.path().join("snapshot");
        let providers = root.path().join("providers");
        std::fs::create_dir_all(&mounts).expect("mounts");
        std::fs::create_dir_all(&providers).expect("providers");
        let wasm = test_provider_wasm_path();
        if !wasm.exists() {
            // Provider WASM is an optional maintainer artifact; the failure
            // tests above remain hermetic when it is absent.
            return;
        }
        let spec = pin_spec(
            &providers,
            &wasm,
            "test-provider",
            serde_json::json!({
                "mount": "test",
                "config": {},
                "capabilities": { "domains": ["httpbin.org"] }
            }),
        );
        std::fs::write(
            mounts.join("test.json"),
            serde_json::to_vec_pretty(&spec).expect("serialize spec"),
        )
        .expect("spec");
        let context = HostContext::new(
            root.path().join("cache"),
            root.path().join("config"),
            &providers,
            root.path().join("credentials"),
        )
        .with_wasm_cache_dir(crate::test_support::wasm_cache_dir());
        let registry = MountRuntimes::load(
            context,
            Arc::new(GitCloner::new(root.path().join("clones"))),
            &Registry::load(&mounts).expect("load selected snapshot"),
            &tokio::runtime::Handle::current(),
        )
        .expect("startup load");
        assert_eq!(registry.mounts(), ["test"]);
        registry.shutdown_all();
    }
}
