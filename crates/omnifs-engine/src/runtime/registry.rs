//! Fixed online/offline mount table and provider lifecycle ownership.
//!
//! Startup is atomic: every selected mount is built and validated in a
//! temporary collection before the fixed table is published.

use crate::auth::credential_service_for_file;
use crate::cache::{Caches, MountResources, ProjectionError, ProjectionId};
use crate::cloner::GitCloner;
use crate::tree_refs::TreeRefs;
use crate::{BuildError, HostContext, Runtime, component_engine};
use omnifs_auth::CredentialService;
use omnifs_workspace::mounts::{LoadedSpec, Name, Registry};
use omnifs_workspace::provider::ProviderWasm;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tracing::{debug, info, warn};

/// One selected mount revision. Cache-only entries deliberately have no
/// provider runtime and never fabricate provider handles.
pub struct MountEntry {
    name: Name,
    identity: LoadedSpec,
    projection_id: ProjectionId,
    resources: Arc<MountResources>,
    trees: Arc<TreeRefs>,
    runtime: Option<Arc<Runtime>>,
}

impl MountEntry {
    pub(crate) fn name(&self) -> &Name {
        &self.name
    }

    pub(crate) fn identity(&self) -> &LoadedSpec {
        &self.identity
    }

    pub(crate) fn projection_id(&self) -> ProjectionId {
        self.projection_id
    }

    pub(crate) fn resources(&self) -> &Arc<MountResources> {
        &self.resources
    }

    pub(crate) fn trees(&self) -> &Arc<TreeRefs> {
        &self.trees
    }

    pub(crate) fn runtime(&self) -> Option<Arc<Runtime>> {
        self.runtime.clone()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TableMode {
    Online,
    Offline,
}

/// Fixed selected mount table used by the single namespace implementation.
pub struct MountTable {
    context: HostContext,
    mode: TableMode,
    entries: BTreeMap<String, MountEntry>,
    timer_shutdown: watch::Sender<bool>,
    timer_tasks: parking_lot::Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
}

impl MountTable {
    /// Load every selected mount with its real provider runtime.
    pub fn load_online(
        context: HostContext,
        cloner: Arc<GitCloner>,
        desired: &Registry,
        handle: &tokio::runtime::Handle,
    ) -> Result<Self, RegistryError> {
        Self::load_online_with_options(context, cloner, desired, handle, false)
    }

    pub(crate) fn load_online_with_options(
        context: HostContext,
        cloner: Arc<GitCloner>,
        desired: &Registry,
        handle: &tokio::runtime::Handle,
        capture_test_callouts: bool,
    ) -> Result<Self, RegistryError> {
        validate_registry(desired)?;
        // Compiled component artifacts live with the rest of the host's state,
        // under `<cache>/wasm`, rather than a global per-user wasmtime cache.
        let wasm_cache = context.wasm_cache_dir();
        let engine = component_engine(Some(wasm_cache), |_| {})
            .map_err(|e| RegistryError::RuntimeError(format!("provider engine init: {e}")))?;

        // One global body store and projection database. Each selected exact
        // projection receives one long-lived MountResources owner.
        let caches = Caches::open(context.cache_dir())
            .map_err(|e| RegistryError::RuntimeError(format!("cache open: {e}")))?;

        // One credential owner for the whole host, shared across every mount.
        let credential_service = credential_service_for_file(context.credentials_file())
            .map_err(|e| RegistryError::RuntimeError(format!("credential service init: {e}")))?;
        let (timer_shutdown, _) = watch::channel(false);
        let built = desired
            .loaded_iter()
            .map(|(name, loaded)| {
                Self::build_online_mount(
                    name,
                    loaded,
                    &engine,
                    &caches,
                    &cloner,
                    &context,
                    &credential_service,
                    capture_test_callouts,
                )
            })
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
        let entries = built
            .iter()
            .map(|built| (built.entry.name.to_string(), built.entry.clone_for_table()))
            .collect();
        let table = Self {
            context,
            mode: TableMode::Online,
            entries,
            timer_shutdown,
            timer_tasks: parking_lot::Mutex::new(HashMap::new()),
        };
        for built in built {
            table.start_timer(
                built.entry.name.as_str(),
                &built.runtime,
                built.provider_interval_secs,
                handle,
            );
            info!(mount = built.entry.name.as_str(), "loaded provider");
        }
        Ok(table)
    }

    #[allow(clippy::too_many_arguments)]
    fn build_online_mount(
        name: &Name,
        loaded: &LoadedSpec,
        engine: &wasmtime::Engine,
        caches: &Arc<Caches>,
        cloner: &Arc<GitCloner>,
        context: &HostContext,
        credential_service: &Arc<CredentialService>,
        capture_test_callouts: bool,
    ) -> Result<BuiltMount, RegistryError> {
        let spec = loaded.spec();
        let mount = name.to_string();
        let wasm_path = context.provider_path_by_id(&spec.provider.id);
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
        let projection_id = ProjectionId::new(loaded.source(), spec.provider.id);
        let resources = caches
            .mount(&name, projection_id, spec.provider.id, loaded.source())
            .map_err(|error| RegistryError::RuntimeError(format!("cache open: {error}")))?;
        let trees = Arc::new(TreeRefs::new());

        let runtime = if capture_test_callouts {
            Runtime::new_for_callout_tests(
                engine,
                &wasm_path,
                spec,
                &manifest,
                Arc::clone(cloner),
                context,
                resources.clone(),
                Arc::clone(&trees),
                credential_service,
            )
        } else {
            Runtime::new(
                engine,
                &wasm_path,
                spec,
                &manifest,
                Arc::clone(cloner),
                context,
                resources.clone(),
                Arc::clone(&trees),
                credential_service,
            )
        }
        .map_err(|error| RegistryError::from_build(&mount, error))?;
        let runtime = Arc::new(runtime);
        Ok(BuiltMount {
            provider_interval_secs,
            runtime: Arc::clone(&runtime),
            entry: MountEntry {
                name: name.clone(),
                identity: loaded.clone(),
                projection_id,
                resources,
                trees,
                runtime: Some(runtime),
            },
        })
    }

    /// Load only complete durable facts for the exact current mount snapshot.
    /// No provider artifact, credential owner, Wasmtime engine, timer, HTTP
    /// client, or Git process is constructed on this path.
    pub fn load_offline(context: HostContext, desired: &Registry) -> Result<Self, RegistryError> {
        validate_registry(desired)?;
        let caches = Caches::open_existing(context.cache_dir())
            .map_err(|error| RegistryError::OfflineCache(error.to_string()))?;
        let mut entries = BTreeMap::new();
        let mut cloner = None;
        for (name, loaded) in desired.loaded_iter() {
            let spec = loaded.spec();
            let projection_id = ProjectionId::new(loaded.source(), spec.provider.id);
            let resources = caches
                .mount_existing(name, projection_id, spec.provider.id, loaded.source())
                .map_err(|error| RegistryError::offline_projection(name, error))?;
            let trees = Arc::new(TreeRefs::new());
            for (_path, fact) in
                resources
                    .git_facts()
                    .map_err(|error| RegistryError::CorruptProjection {
                        mount: name.to_string(),
                        message: error.to_string(),
                    })?
            {
                let cloner = match &cloner {
                    Some(cloner) => cloner,
                    None => {
                        let opened =
                            GitCloner::open_existing(context.cache_dir().join("clones"))
                                .map_err(|error| RegistryError::OfflineCache(error.to_string()))?;
                        cloner.insert(opened)
                    },
                };
                let repo = cloner
                    .open_cached(name.as_str(), &fact.id, &fact.relative_path)
                    .map_err(|error| RegistryError::OfflineGit {
                        mount: name.to_string(),
                        message: error.to_string(),
                    })?;
                trees
                    .open(fact.id, &fact.relative_path, &repo)
                    .map_err(|error| RegistryError::OfflineGit {
                        mount: name.to_string(),
                        message: error.to_string(),
                    })?;
            }
            entries.insert(
                name.to_string(),
                MountEntry {
                    name: name.clone(),
                    identity: loaded.clone(),
                    projection_id,
                    resources,
                    trees,
                    runtime: None,
                },
            );
        }
        let (timer_shutdown, _) = watch::channel(false);
        Ok(Self {
            context,
            mode: TableMode::Offline,
            entries,
            timer_shutdown,
            timer_tasks: parking_lot::Mutex::new(HashMap::new()),
        })
    }

    /// Host context this registry resolves mounts against.
    pub fn context(&self) -> &HostContext {
        &self.context
    }

    /// The immutable runtime for one loaded mount.
    pub fn get(&self, mount: &str) -> Option<Arc<Runtime>> {
        self.entries.get(mount).and_then(MountEntry::runtime)
    }

    pub(crate) fn entry(&self, mount: &str) -> Option<&MountEntry> {
        self.entries.get(mount)
    }

    pub(crate) fn is_offline(&self) -> bool {
        self.mode == TableMode::Offline
    }

    pub fn mounts(&self) -> Vec<String> {
        self.entries.keys().cloned().collect()
    }

    pub fn runtime_entries(&self) -> Vec<(String, Arc<Runtime>)> {
        self.entries
            .iter()
            .filter_map(|(mount, entry)| entry.runtime().map(|runtime| (mount.clone(), runtime)))
            .collect()
    }

    /// The selected identity and optional provider runtime for every mount.
    pub fn selected_entries(
        &self,
    ) -> impl Iterator<Item = (&Name, &LoadedSpec, Option<Arc<Runtime>>)> + '_ {
        self.entries
            .values()
            .map(|entry| (&entry.name, &entry.identity, entry.runtime()))
    }

    pub fn shutdown_all(&self) {
        let _ = self.timer_shutdown.send(true);
        for (_, task) in self.timer_tasks.lock().drain() {
            task.abort();
        }
        for (mount, entry) in &self.entries {
            if let Some(runtime) = entry.runtime()
                && let Err(e) = runtime.shutdown()
            {
                warn!(mount, error = %e, "shutdown failed");
            }
        }
    }

    fn start_timer(
        &self,
        mount: &str,
        runtime: &Arc<Runtime>,
        provider_interval_secs: u32,
        handle: &tokio::runtime::Handle,
    ) {
        if provider_interval_secs == 0 {
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
                let period = Duration::from_secs(u64::from(provider_interval_secs));
                let mut interval = tokio::time::interval_at(
                    tokio::time::Instant::now() + period,
                    period,
                );
                loop {
                    tokio::select! {
                        _ = interval.tick() => {
                            if let Err(e) = runtime.call_timer_tick().await
                            {
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

struct BuiltMount {
    entry: MountEntry,
    provider_interval_secs: u32,
    runtime: Arc<Runtime>,
}

impl MountEntry {
    fn clone_for_table(&self) -> Self {
        Self {
            name: self.name.clone(),
            identity: self.identity.clone(),
            projection_id: self.projection_id,
            resources: Arc::clone(&self.resources),
            trees: Arc::clone(&self.trees),
            runtime: self.runtime.clone(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("config error: {0}")]
    ConfigError(String),
    #[error("provider not found: {0}")]
    ProviderNotFound(String),
    #[error("runtime error: {0}")]
    RuntimeError(String),
    #[error("offline cache open failed: {0}")]
    OfflineCache(String),
    #[error("mount {mount} has no durable projection for its exact spec and provider identity")]
    MissingProjection { mount: String },
    #[error("mount {mount} has a corrupt durable projection: {message}")]
    CorruptProjection { mount: String, message: String },
    #[error("mount {mount} has an invalid cached Git tree: {message}")]
    OfflineGit { mount: String, message: String },
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

    fn offline_projection(mount: &Name, error: ProjectionError) -> Self {
        if matches!(
            error,
            ProjectionError::Store(crate::cache::projection::ProjectionStoreError::Missing)
        ) {
            Self::MissingProjection {
                mount: mount.to_string(),
            }
        } else {
            Self::CorruptProjection {
                mount: mount.to_string(),
                message: error.to_string(),
            }
        }
    }
}

fn validate_registry(desired: &Registry) -> Result<(), RegistryError> {
    if let Some(failure) = desired.failures().first() {
        return Err(RegistryError::ConfigError(format!(
            "load mount spec {}: {}",
            failure.path.display(),
            failure.error
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{MountTable, RegistryError};
    use crate::HostContext;
    use crate::cloner::GitCloner;
    use omnifs_workspace::ids::ProviderId;
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

    fn run_git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "offline-test")
            .env("GIT_AUTHOR_EMAIL", "offline@test.invalid")
            .env("GIT_COMMITTER_NAME", "offline-test")
            .env("GIT_COMMITTER_EMAIL", "offline@test.invalid")
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed");
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

        let result = MountTable::load_online(
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
        let result = MountTable::load_online(
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
        let result = MountTable::load_online(
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
                        "client_id": client_id
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
        let result = MountTable::load_online(
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
        let registry = MountTable::load_online(
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

    #[tokio::test(flavor = "multi_thread")]
    async fn offline_table_serves_complete_projection_and_fails_closed() {
        use crate::cache::identity::{GitId, ProjectionId};
        use crate::cache::mount::{
            Caches, DirentsMutation, FactPayload, Freshness, GitWrite, ObjectMutation,
            ProjectionTransition, RecordWrite,
        };
        use crate::namespace::{DirCursor, Namespace, NsError};
        use crate::object_id::ObjectId;
        use crate::view::{
            CachedCursor, DirentRecord, DirentsPayload, EntryMeta, FileAttrsCache, FilePayload,
            FileSize, LookupPayload, Stability,
        };
        use crate::{TreeNamespace, view::BodyId};
        use fjall::{KeyspaceCreateOptions, PersistMode};
        use omnifs_core::path::Path as ProjectedPath;
        use omnifs_wit::provider::types::{IdCapture, LogicalId};

        let root = tempfile::tempdir().expect("offline fixture root");
        let cache = root.path().join("cache");
        let mounts = root.path().join("snapshot");
        std::fs::create_dir_all(&mounts).expect("mount snapshot");
        let provider_id = ProviderId::from_wasm_bytes(b"offline provider identity");
        let spec: Spec = serde_json::from_value(serde_json::json!({
            "provider": {
                "id": provider_id,
                "meta": { "name": "offline-only" }
            },
            "mount": "test",
            "config": {}
        }))
        .expect("offline mount spec");
        let mut desired = Registry::load(&mounts).expect("load mount snapshot");
        desired.put(&spec).expect("write exact mount bytes");
        let mut root_git_spec = spec.clone();
        root_git_spec.mount = "rootgit".to_string();
        desired
            .put(&root_git_spec)
            .expect("write root Git mount bytes");
        let context = HostContext::new(
            &cache,
            root.path().join("config"),
            root.path().join("providers-missing"),
            root.path().join("credentials-missing"),
        );
        let (_, loaded) = desired
            .loaded_iter()
            .find(|(name, _)| name.as_str() == "test")
            .expect("selected mount");
        let projection_id = ProjectionId::new(loaded.source(), provider_id);
        let caches = Caches::open(&cache).expect("online projection owner");
        let resources = caches
            .mount(
                &omnifs_workspace::mounts::Name::new("test").unwrap(),
                projection_id,
                provider_id,
                loaded.source(),
            )
            .expect("selected projection");
        let (_, root_git_loaded) = desired
            .loaded_iter()
            .find(|(name, _)| name.as_str() == "rootgit")
            .expect("root Git mount");
        let root_git_projection =
            ProjectionId::new(root_git_loaded.source(), root_git_loaded.spec().provider.id);
        let root_git_resources = caches
            .mount(
                &omnifs_workspace::mounts::Name::new("rootgit").unwrap(),
                root_git_projection,
                root_git_loaded.spec().provider.id,
                root_git_loaded.source(),
            )
            .expect("root Git projection");

        let source_repo = root.path().join("source-repo");
        std::fs::create_dir_all(source_repo.join("src")).expect("source repository");
        std::fs::write(source_repo.join("src/main.rs"), b"fn cached() {}\n").expect("Git body");
        run_git(&source_repo, &["init", "-b", "main"]);
        run_git(&source_repo, &["add", "."]);
        run_git(&source_repo, &["commit", "-m", "offline fixture"]);
        let remote = "https://fixture.test/offline.git";
        let git_id = GitId::new("test", remote, Some("main"));
        let cloner = GitCloner::new(cache.join("clones")).expect("online clone owner");
        cloner
            .clone_if_needed(
                &git_id,
                &source_repo.to_string_lossy(),
                remote,
                Some("main"),
                1,
            )
            .expect("warm Git cache");
        let root_git_id = GitId::new("rootgit", remote, Some("main"));
        cloner
            .clone_if_needed(
                &root_git_id,
                &source_repo.to_string_lossy(),
                remote,
                Some("main"),
                2,
            )
            .expect("warm root Git cache");
        root_git_resources
            .publish(
                ProjectionTransition {
                    records: vec![RecordWrite {
                        path: ProjectedPath::root(),
                        aux: None,
                        fact: FactPayload::Lookup(LookupPayload::Positive(EntryMeta::directory())),
                    }],
                    git: vec![GitWrite {
                        path: ProjectedPath::root(),
                        id: root_git_id,
                        relative_path: "src".into(),
                    }],
                    ..ProjectionTransition::default()
                },
                root_git_resources.current_epoch(),
            )
            .expect("publish mount-root Git handoff");

        let inline_path = ProjectedPath::parse("/inline.txt").unwrap();
        let canonical_path = ProjectedPath::parse("/canonical.txt").unwrap();
        let git_path = ProjectedPath::parse("/git").unwrap();
        let partial_path = ProjectedPath::parse("/partial").unwrap();
        let negative_path = ProjectedPath::parse("/partial/gone").unwrap();
        let inline_bytes = b"offline inline\n".to_vec();
        let canonical_bytes = b"offline canonical\n".to_vec();
        let canonical_id = ObjectId::from_wit(&LogicalId {
            kind: "issue".into(),
            captures: vec![IdCapture {
                name: "number".into(),
                value: "42".into(),
            }],
        })
        .as_bytes()
        .to_vec();
        let inline_meta = EntryMeta::file(
            FileAttrsCache::inline(inline_bytes.clone(), Stability::Dynamic, None).unwrap(),
        );
        let canonical_meta = EntryMeta::file(
            FileAttrsCache::canonical(
                FileSize::Exact(canonical_bytes.len() as u64),
                Stability::Dynamic,
                None,
            )
            .unwrap(),
        );
        let root_entries = vec![
            DirentRecord {
                name: "inline.txt".into(),
                meta: inline_meta.clone(),
            },
            DirentRecord {
                name: "canonical.txt".into(),
                meta: canonical_meta.clone(),
            },
            DirentRecord {
                name: "git".into(),
                meta: EntryMeta::directory(),
            },
            DirentRecord {
                name: "partial".into(),
                meta: EntryMeta::directory(),
            },
        ];
        resources
            .publish(
                ProjectionTransition {
                    records: vec![
                        RecordWrite {
                            path: inline_path.clone(),
                            aux: None,
                            fact: FactPayload::Lookup(LookupPayload::Positive(inline_meta.clone())),
                        },
                        RecordWrite {
                            path: inline_path.clone(),
                            aux: None,
                            fact: FactPayload::File(FilePayload::new(None, inline_bytes.clone())),
                        },
                        RecordWrite {
                            path: canonical_path.clone(),
                            aux: None,
                            fact: FactPayload::Lookup(LookupPayload::Positive(
                                canonical_meta.clone(),
                            )),
                        },
                        RecordWrite {
                            path: git_path.clone(),
                            aux: None,
                            fact: FactPayload::Lookup(LookupPayload::Positive(
                                EntryMeta::directory(),
                            )),
                        },
                        RecordWrite {
                            path: partial_path.clone(),
                            aux: None,
                            fact: FactPayload::Lookup(LookupPayload::Positive(
                                EntryMeta::directory(),
                            )),
                        },
                        RecordWrite {
                            path: negative_path.clone(),
                            aux: None,
                            fact: FactPayload::Lookup(LookupPayload::Negative { id: None }),
                        },
                    ],
                    dirents: vec![
                        DirentsMutation::Replace {
                            path: ProjectedPath::root(),
                            value: DirentsPayload {
                                entries: root_entries,
                                exhaustive: true,
                                validator: None,
                                next_cursor: None,
                                paginated: false,
                            },
                        },
                        DirentsMutation::Replace {
                            path: partial_path.clone(),
                            value: DirentsPayload {
                                entries: Vec::new(),
                                exhaustive: false,
                                validator: None,
                                next_cursor: Some(CachedCursor::Page(1)),
                                paginated: true,
                            },
                        },
                    ],
                    objects: vec![
                        ObjectMutation::Canonical {
                            id: canonical_id.clone(),
                            bytes: canonical_bytes.clone(),
                            validator: None,
                        },
                        ObjectMutation::Index {
                            id: canonical_id,
                            alias: canonical_path.clone(),
                        },
                    ],
                    freshness: [
                        &inline_path,
                        &canonical_path,
                        &git_path,
                        &partial_path,
                        &negative_path,
                    ]
                    .into_iter()
                    .map(|path| Freshness {
                        path: path.clone(),
                        expires_at: Some(1),
                    })
                    .collect(),
                    git: vec![GitWrite {
                        path: git_path,
                        id: git_id,
                        relative_path: "src".into(),
                    }],
                    ..ProjectionTransition::default()
                },
                resources.current_epoch(),
            )
            .expect("publish complete durable projection");
        drop(resources);
        drop(root_git_resources);
        drop(caches);

        let table = Arc::new(
            MountTable::load_offline(context.clone(), &desired).expect("offline table startup"),
        );
        assert!(table.get("test").is_none());
        let namespace = TreeNamespace::offline(table, tokio::runtime::Handle::current());
        let mount = namespace
            .lookup(ProjectedPath::root(), "test")
            .await
            .expect("offline mount");
        let listing = namespace
            .readdir(mount.path.clone(), DirCursor::start(), 0)
            .await
            .expect("complete expired listing");
        assert_eq!(
            listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            [
                "inline.txt",
                "canonical.txt",
                "git",
                "partial",
                ".gitignore",
                ".ignore",
                ".rgignore",
            ]
        );
        assert!(matches!(
            namespace
                .readdir(
                    mount.path.clone(),
                    DirCursor::Buffered {
                        entries: listing.entries.clone(),
                        then: None,
                        offline: false,
                    },
                    0,
                )
                .await,
            Err(NsError::OfflineMiss)
        ));
        let inline = namespace
            .lookup(mount.path.clone(), "inline.txt")
            .await
            .expect("expired inline lookup");
        assert_eq!(
            Namespace::read(namespace.as_ref(), inline.path, 0, 1024)
                .await
                .unwrap()
                .bytes,
            inline_bytes
        );
        let canonical = namespace
            .lookup(mount.path.clone(), "canonical.txt")
            .await
            .expect("expired canonical lookup");
        assert_eq!(
            Namespace::read(namespace.as_ref(), canonical.path, 0, 1024)
                .await
                .unwrap()
                .bytes,
            canonical_bytes
        );
        let git = namespace
            .lookup(mount.path.clone(), "git")
            .await
            .expect("offline Git subtree");
        let git_file = namespace
            .lookup(git.path, "main.rs")
            .await
            .expect("offline Git child");
        assert_eq!(
            Namespace::read(namespace.as_ref(), git_file.path, 0, 1024)
                .await
                .unwrap()
                .bytes,
            b"fn cached() {}\n"
        );
        let partial = namespace
            .lookup(mount.path.clone(), "partial")
            .await
            .expect("partial directory identity");
        assert!(matches!(
            namespace.lookup(partial.path.clone(), "unknown").await,
            Err(NsError::OfflineMiss)
        ));
        assert!(matches!(
            namespace.lookup(partial.path, "gone").await,
            Err(NsError::NotFound)
        ));
        assert!(matches!(
            namespace.lookup(mount.path, "unknown").await,
            Err(NsError::NotFound)
        ));
        let root_git_mount = namespace
            .lookup(ProjectedPath::root(), "rootgit")
            .await
            .expect("root Git mount");
        let root_git_listing = namespace
            .readdir(root_git_mount.path.clone(), DirCursor::start(), 0)
            .await
            .expect("mount-root Git listing");
        assert_eq!(root_git_listing.entries[0].name, "main.rs");
        let root_git_file = namespace
            .lookup(root_git_mount.path, "main.rs")
            .await
            .expect("mount-root Git child");
        assert_eq!(
            Namespace::read(namespace.as_ref(), root_git_file.path, 0, 1024)
                .await
                .unwrap()
                .bytes,
            b"fn cached() {}\n"
        );
        drop(namespace);

        let inline_body = BodyId::from_bytes(&inline_bytes);
        let body_path = cache.join("bodies").join(inline_body.hex());
        std::fs::write(&body_path, vec![b'x'; inline_bytes.len()]).expect("corrupt body");
        assert!(matches!(
            MountTable::load_offline(context.clone(), &desired),
            Err(RegistryError::CorruptProjection { .. })
        ));
        std::fs::write(&body_path, &inline_bytes).expect("restore body");

        let database = fjall::OptimisticTxDatabase::builder(cache.join("projections/database"))
            .open()
            .expect("open projection database for corruption witness");
        let facts = database
            .keyspace(
                &format!("facts.{}", projection_id.hex()),
                KeyspaceCreateOptions::default,
            )
            .unwrap();
        let mut tx = database
            .write_tx()
            .unwrap()
            .durability(Some(PersistMode::SyncAll));
        tx.insert(&facts, b"i:/canonical.txt", [0xff]);
        assert!(tx.commit().unwrap().is_ok());
        drop(facts);
        drop(database);
        match MountTable::load_offline(context, &desired) {
            Err(RegistryError::CorruptProjection { .. }) => {},
            Err(error) => panic!("expected corrupt projection, got {error:?}"),
            Ok(_) => panic!("expected corrupt projection, got a table"),
        }
    }
}
