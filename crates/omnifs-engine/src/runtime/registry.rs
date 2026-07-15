//! Provider registry: startup loading and lifecycle management for WASM providers.
//!
//! Startup is atomic: the complete immutable mount snapshot is scanned,
//! snapshotted, and instantiated before any runtime is published.

use crate::auth::credential_service_for_file;
use crate::cache::Caches;
use crate::cloner::GitCloner;
use crate::{BuildError, HostContext, Runtime, component_engine};
use omnifs_auth::CredentialService;
use omnifs_workspace::mounts::{Registry, Spec};
use omnifs_workspace::provider::ProviderWasm;
use std::collections::HashMap;
use std::fs;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tracing::{debug, info, warn};

pub(crate) const DEFAULT_REVALIDATE_SECS: u64 = 15 * 60;

/// Registry of loaded WASM providers.
///
/// Instantiates providers on demand and manages their lifecycle including
/// per-mount timer-driven revalidation tasks.
pub struct MountRuntimes {
    engine: wasmtime::Engine,
    caches: Arc<Caches>,
    cloner: Arc<GitCloner>,
    context: HostContext,
    /// Shared cache owner and OAuth transport retained for the lifetime of all
    /// mount-owned bindings.
    credential_service: Arc<CredentialService>,
    instances: HashMap<String, Arc<Runtime>>,
    timer_shutdown: watch::Sender<bool>,
    timer_tasks: parking_lot::Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
}

impl MountRuntimes {
    fn initialize(context: HostContext, cloner: Arc<GitCloner>) -> Result<Self, RegistryError> {
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
            instances: HashMap::new(),
            timer_shutdown,
            timer_tasks: parking_lot::Mutex::new(HashMap::new()),
        })
    }

    /// Load and publish every mount from the supplied immutable snapshot. Any
    /// snapshot assembly, artifact, duplicate, or runtime error aborts
    /// startup before the first runtime is visible.
    pub fn load(
        context: HostContext,
        cloner: Arc<GitCloner>,
        desired: &Registry,
        handle: &tokio::runtime::Handle,
    ) -> Result<Self, RegistryError> {
        Self::load_with_options(context, cloner, desired, handle, false)
    }

    pub(crate) fn load_with_options(
        context: HostContext,
        cloner: Arc<GitCloner>,
        desired: &Registry,
        handle: &tokio::runtime::Handle,
        capture_test_callouts: bool,
    ) -> Result<Self, RegistryError> {
        let mut registry = Self::initialize(context, cloner)?;
        if let Some(failure) = desired.failures().first() {
            return Err(RegistryError::ConfigError(format!(
                "load mount spec {}: {}",
                failure.path.display(),
                failure.error
            )));
        }
        let built = desired
            .iter()
            .map(|(_, spec)| registry.build_mount(spec, capture_test_callouts))
            .collect::<Result<Vec<_>, _>>()?;
        for (index, left) in built.iter().enumerate() {
            for right in built.iter().skip(index + 1) {
                if let (Some(left), Some(right)) =
                    (left.runtime.auth_binding(), right.runtime.auth_binding())
                {
                    if left.credential_id() == right.credential_id() && !left.same_runtime_as(right)
                    {
                        return Err(RegistryError::ConfigError(
                            omnifs_auth::AuthError::CredentialBindingConflict {
                                id: left.credential_id().clone(),
                            }
                            .to_string(),
                        ));
                    }
                }
            }
        }
        registry.instances = built
            .iter()
            .map(|built| (built.mount.clone(), Arc::clone(&built.runtime)))
            .collect();
        for built in built {
            registry.start_timer(
                &built.mount,
                &built.runtime,
                built.provider_interval_secs,
                built.revalidate,
                handle,
            );
            info!(mount = built.mount.as_str(), "loaded provider");
        }
        Ok(registry)
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

        let manifest = fs::read(&wasm_path)
            .map_err(|error| {
                RegistryError::RuntimeError(format!(
                    "reading provider manifest {}: {error}",
                    wasm_path.display()
                ))
            })
            .and_then(|bytes| {
                ProviderWasm::from_bytes(bytes).metadata().map_err(|error| {
                    RegistryError::RuntimeError(format!(
                        "reading provider manifest {}: {error}",
                        wasm_path.display()
                    ))
                })
            })
            .and_then(|manifest| {
                manifest.ok_or_else(|| {
                    RegistryError::RuntimeError(format!(
                        "provider artifact {} has no embedded manifest",
                        wasm_path.display()
                    ))
                })
            })?;
        let provider_interval_secs = manifest.refresh_interval_secs;

        let runtime = if capture_test_callouts {
            Runtime::new_for_callout_tests(
                &self.engine,
                &wasm_path,
                spec,
                &manifest,
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
                &manifest,
                self.cloner.clone(),
                &self.context,
                &self.caches,
                &self.credential_service,
            )
        }
        .map_err(|error| RegistryError::from_build(&mount, error))?;
        Ok(BuiltMount {
            mount,
            provider_interval_secs,
            revalidate: spec.revalidate,
            runtime: Arc::new(runtime),
        })
    }

    /// Host context this registry resolves mounts against.
    pub fn context(&self) -> &HostContext {
        &self.context
    }

    /// The immutable runtime for one loaded mount.
    pub fn get(&self, mount: &str) -> Option<Arc<Runtime>> {
        self.instances.get(mount).cloned()
    }

    pub fn mounts(&self) -> Vec<String> {
        self.instances.keys().cloned().collect()
    }

    pub fn runtime_entries(&self) -> Vec<(String, Arc<Runtime>)> {
        self.instances
            .iter()
            .map(|(mount, runtime)| (mount.clone(), Arc::clone(runtime)))
            .collect()
    }

    pub fn shutdown_all(&self) {
        let _ = self.timer_shutdown.send(true);
        for (_, task) in self.timer_tasks.lock().drain() {
            task.abort();
        }
        for (mount, runtime) in &self.instances {
            if let Err(e) = runtime.shutdown() {
                warn!(mount, error = %e, "shutdown failed");
            }
        }
    }

    fn start_timer(
        &self,
        mount: &str,
        runtime: &Arc<Runtime>,
        provider_interval_secs: u32,
        revalidate: bool,
        handle: &tokio::runtime::Handle,
    ) {
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
    provider_interval_secs: u32,
    revalidate: bool,
    runtime: Arc<Runtime>,
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("config error: {0}")]
    ConfigError(String),
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
    use omnifs_workspace::mounts::{Registry, Spec};
    use omnifs_workspace::provider::{Artifact, ProviderStore};
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
        let artifact = Artifact::from_bytes(format!("{name}.wasm"), bytes.to_vec())
            .expect("parse provider artifact");
        let reference = artifact.reference();
        let store = ProviderStore::new(providers_dir);
        store.retain(&artifact).expect("retain provider");
        body["provider"] = serde_json::to_value(reference).expect("serialize provider reference");
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
    async fn load_rejects_invalid_provider_config() {
        // The test provider's embedded manifest declares empty config metadata,
        // so the host's validate_instance_config rejects a mount config with
        // extra fields.
        let config_dir = tempfile::tempdir().expect("temp config dir");
        let cache_dir = tempfile::tempdir().expect("temp cache dir");
        let mounts_dir = tempfile::tempdir().expect("temp mounts dir");
        let providers_dir = tempfile::tempdir().expect("temp providers dir");
        let paths = omnifs_workspace::layout::WorkspaceLayout::under_root(config_dir.path());

        let base_wasm = test_provider_wasm_path();
        assert!(
            base_wasm.exists(),
            "test provider missing at {}. Run `just build providers` first.",
            base_wasm.display()
        );

        // Pin the test provider into the provider store, then mount it with an
        // out-of-schema config field. Config validation must fail before
        // provider instance construction.
        let spec = pin_spec(
            providers_dir.path(),
            &base_wasm,
            "test-provider",
            serde_json::json!({
                "mount": "test",
                "config": { "unexpected": true }
            }),
        );
        std::fs::write(
            mounts_dir.path().join("test.json"),
            serde_json::to_vec_pretty(&spec).expect("serialize spec"),
        )
        .expect("write spec");

        let result = MountRuntimes::load(
            HostContext::new(
                cache_dir.path(),
                &paths.config_dir,
                providers_dir.path(),
                &paths.credentials_file,
            )
            .with_wasm_cache_dir(crate::test_support::wasm_cache_dir()),
            Arc::new(GitCloner::new(cache_dir.path().join("clones")).unwrap()),
            &Registry::load(mounts_dir.path()).expect("load selected snapshot"),
            &tokio::runtime::Handle::current(),
        );
        match result {
            Err(RegistryError::ConfigError(message)) => {
                assert!(message.contains("failed validation"));
                assert!(message.contains("mount test"));
            },
            Err(other) => panic!("expected config error, got {other}"),
            Ok(_) => panic!("expected invalid provider config to be rejected"),
        }
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
            Arc::new(GitCloner::new(root.path().join("clones")).unwrap()),
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
            Arc::new(GitCloner::new(root.path().join("clones")).unwrap()),
            &Registry::load(&mounts).expect("load selected snapshot"),
            &tokio::runtime::Handle::current(),
        );
        assert!(
            matches!(result, Err(RegistryError::ConfigError(message)) if message.contains("broken.json"))
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn load_rejects_conflicting_shared_oauth_bindings_before_publication() {
        let root = tempfile::tempdir().expect("temp root");
        let mounts = root.path().join("snapshot");
        let providers = root.path().join("providers");
        std::fs::create_dir_all(&mounts).expect("mounts");
        std::fs::create_dir_all(&providers).expect("providers");
        let wasm = wasm_artifact_path("omnifs_provider_github.wasm");
        assert!(
            wasm.exists(),
            "GitHub provider missing at {}. Run `just build providers` first.",
            wasm.display()
        );

        for (mount, client_id) in [("left", "client-a"), ("right", "client-b")] {
            let spec = pin_spec(
                &providers,
                &wasm,
                "github",
                serde_json::json!({
                    "mount": mount,
                    "config": {},
                    "auth": {
                        "type": "oauth",
                        "scheme": "device",
                        "account": "shared",
                        "clientId": client_id
                    }
                }),
            );
            std::fs::write(
                mounts.join(format!("{mount}.json")),
                serde_json::to_vec_pretty(&spec).expect("serialize spec"),
            )
            .expect("write spec");
        }

        let context = HostContext::new(
            root.path().join("cache"),
            root.path().join("config"),
            &providers,
            root.path().join("credentials"),
        )
        .with_wasm_cache_dir(crate::test_support::wasm_cache_dir());
        let result = MountRuntimes::load(
            context,
            Arc::new(GitCloner::new(root.path().join("clones")).unwrap()),
            &Registry::load(&mounts).expect("load selected snapshot"),
            &tokio::runtime::Handle::current(),
        );

        assert!(matches!(
            result,
            Err(RegistryError::ConfigError(message))
                if message.contains("conflicting OAuth runtime metadata")
        ));
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
            Arc::new(GitCloner::new(root.path().join("clones")).unwrap()),
            &Registry::load(&mounts).expect("load selected snapshot"),
            &tokio::runtime::Handle::current(),
        )
        .expect("startup load");
        assert_eq!(registry.mounts(), ["test"]);
        assert_eq!(registry.timer_tasks.lock().len(), 1);
        registry.shutdown_all();
    }
}
