//! Fixed mount table and provider lifecycle ownership.
//!
//! Startup is atomic: every selected mount is built and validated in a
//! temporary collection before the fixed table is published.

use crate::cache::{MountResources, ProjectionId};
use crate::runtime::RuntimeBuildInput;
use crate::runtime::host::HostOnline;
use crate::tree_refs::TreeRefs;
use crate::{BuildError, Runtime, RuntimeMountConfig};
use omnifs_auth::AuthBinding;
use omnifs_core::{CredentialGeneration, ProviderId, ResourceName};
use omnifs_provider::ProviderManifest;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::watch;
use tracing::{debug, warn};

/// One selected mount revision. Cache-only entries deliberately have no
/// provider runtime and never fabricate provider handles.
pub struct MountEntry {
    name: ResourceName,
    config: RuntimeMountConfig,
    availability: MountAvailability,
    resources: Arc<MountResources>,
    trees: Arc<TreeRefs>,
    runtime: Option<Arc<Runtime>>,
    provider_interval_secs: u32,
}

/// Daemon-supplied, already-validated input for one durable mount.
pub struct MountBuildInput {
    pub config: RuntimeMountConfig,
    pub canonical: Arc<[u8]>,
    pub provider: Option<ProviderBuildInput>,
    pub state: MountBuildState,
}

pub struct ProviderBuildInput {
    pub bytes: Arc<[u8]>,
    pub manifest: ProviderManifest,
}

pub enum MountBuildState {
    Active {
        auth: Option<Arc<AuthBinding>>,
        credential_generation: Option<CredentialGeneration>,
    },
    AuthRequired,
    ProviderUnavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountAvailability {
    Active,
    AuthRequired,
    ProviderUnavailable,
}

impl MountEntry {
    pub(crate) fn resources(&self) -> &Arc<MountResources> {
        &self.resources
    }

    pub(crate) fn trees(&self) -> &Arc<TreeRefs> {
        &self.trees
    }

    pub(crate) fn runtime(&self) -> Option<Arc<Runtime>> {
        self.runtime.clone()
    }

    pub(crate) fn availability(&self) -> MountAvailability {
        self.availability
    }
}

/// Fixed selected mount table used by the single namespace implementation.
pub struct MountTable {
    entries: BTreeMap<String, MountEntry>,
    timer_shutdown: watch::Sender<bool>,
    timer_tasks: parking_lot::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    timers_active: AtomicBool,
    shutdown: AtomicBool,
}

impl MountTable {
    /// Build a complete daemon-owned generation without starting timers.
    pub fn prepare_durable(
        host: &HostOnline,
        inputs: Vec<MountBuildInput>,
    ) -> Result<Self, RegistryError> {
        Self::prepare_durable_with_options(host, inputs, false)
    }

    pub(crate) fn prepare_durable_with_options(
        host: &HostOnline,
        inputs: Vec<MountBuildInput>,
        capture_test_callouts: bool,
    ) -> Result<Self, RegistryError> {
        let (timer_shutdown, _) = watch::channel(false);
        let built = inputs
            .into_iter()
            .map(|input| Self::build_durable_mount(host, input, capture_test_callouts))
            .collect::<Result<Vec<_>, _>>()?;
        validate_auth_bindings(&built)?;
        let entries = built
            .into_iter()
            .map(|entry| (entry.name.to_string(), entry))
            .collect();
        Ok(Self {
            entries,
            timer_shutdown,
            timer_tasks: parking_lot::Mutex::new(Vec::new()),
            timers_active: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
        })
    }

    fn build_durable_mount(
        host: &HostOnline,
        input: MountBuildInput,
        capture_test_callouts: bool,
    ) -> Result<MountEntry, RegistryError> {
        let MountBuildInput {
            config,
            canonical,
            provider,
            state,
        } = input;
        let mount = config.name.to_string();
        let (availability, auth, credential_generation) = match state {
            MountBuildState::Active {
                auth,
                credential_generation,
            } => (MountAvailability::Active, auth, credential_generation),
            MountBuildState::AuthRequired => (MountAvailability::AuthRequired, None, None),
            MountBuildState::ProviderUnavailable => {
                (MountAvailability::ProviderUnavailable, None, None)
            },
        };
        if availability != MountAvailability::ProviderUnavailable {
            let provider = provider.as_ref().ok_or_else(|| {
                RegistryError::RuntimeError(format!(
                    "mount {mount} has no retained provider artifact"
                ))
            })?;
            validate_provider_input(&config, &provider.bytes, &provider.manifest)?;
        }
        let source = generation_cache_source(&canonical, credential_generation);
        let projection_id = ProjectionId::new(&source, config.provider.id);
        let resources = host
            .caches()
            .prepare_mount(&config.name, projection_id, config.provider.id, &source)
            .map_err(|error| RegistryError::RuntimeError(format!("cache open: {error}")))?;
        let trees = Arc::new(TreeRefs::new());
        let provider_interval_secs = provider
            .as_ref()
            .map_or(0, |provider| provider.manifest.refresh_interval_secs);
        let runtime = if availability == MountAvailability::Active {
            let provider = provider.expect("active mount provider was checked above");
            Some(Arc::new(
                Runtime::build(
                    host.engine(),
                    RuntimeBuildInput {
                        wasm: provider.bytes,
                        config: &config,
                        manifest: &provider.manifest,
                        auth,
                        resources: Arc::clone(&resources),
                        trees: Arc::clone(&trees),
                        publish_initialize_effects: false,
                    },
                    Arc::clone(host.cloner()),
                    capture_test_callouts,
                )
                .map_err(|error| RegistryError::from_build(&mount, error))?,
            ))
        } else {
            None
        };
        Ok(MountEntry {
            name: config.name.clone(),
            config,
            availability,
            resources,
            trees,
            runtime,
            provider_interval_secs,
        })
    }

    /// The immutable runtime for one loaded mount.
    pub fn get(&self, mount: &str) -> Option<Arc<Runtime>> {
        self.entries.get(mount).and_then(MountEntry::runtime)
    }

    pub(crate) fn entry(&self, mount: &str) -> Option<&MountEntry> {
        self.entries.get(mount)
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
    ) -> impl Iterator<
        Item = (
            &ResourceName,
            &RuntimeMountConfig,
            MountAvailability,
            Option<Arc<Runtime>>,
        ),
    > + '_ {
        self.entries.values().map(|entry| {
            (
                &entry.name,
                &entry.config,
                entry.availability(),
                entry.runtime(),
            )
        })
    }

    pub fn shutdown_all(&self) {
        if self.shutdown.swap(true, Ordering::AcqRel) {
            return;
        }
        self.begin_retirement();
        for task in self.timer_tasks.lock().drain(..) {
            task.abort();
        }
        self.shutdown_runtimes();
    }

    pub(crate) fn begin_retirement(&self) {
        let _ = self.timer_shutdown.send(true);
    }

    pub(crate) async fn shutdown_all_joined(&self, deadline: tokio::time::Instant) {
        if self.shutdown.swap(true, Ordering::AcqRel) {
            return;
        }
        self.begin_retirement();
        let mut tasks = std::mem::take(&mut *self.timer_tasks.lock());
        if tokio::time::timeout_at(deadline, async {
            for task in &mut tasks {
                let _ = task.await;
            }
        })
        .await
        .is_err()
        {
            for task in &tasks {
                task.abort();
            }
            for task in tasks {
                let _ = task.await;
            }
        }
        self.shutdown_runtimes();
    }

    fn shutdown_runtimes(&self) {
        for (mount, entry) in &self.entries {
            if let Some(runtime) = entry.runtime()
                && let Err(e) = runtime.shutdown()
            {
                warn!(mount, error = %e, "shutdown failed");
            }
        }
    }

    pub(crate) fn activate_resources(&self) {
        for entry in self.entries.values() {
            entry.resources.activate();
        }
    }

    pub(crate) fn retire_resources(&self) {
        for entry in self.entries.values() {
            entry.resources.retire();
        }
    }

    /// Start generation-owned timers exactly once after durable commit.
    pub fn activate_timers(&self, handle: &tokio::runtime::Handle) {
        if self
            .timers_active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        for entry in self.entries.values() {
            if let Some(runtime) = entry.runtime() {
                self.start_timer(
                    entry.name.as_str(),
                    &runtime,
                    entry.provider_interval_secs,
                    handle,
                );
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
        self.timer_tasks.lock().push(task);
    }
}

impl Drop for MountTable {
    fn drop(&mut self) {
        self.shutdown_all();
    }
}

fn validate_auth_bindings(built: &[MountEntry]) -> Result<(), RegistryError> {
    for (index, left) in built.iter().enumerate() {
        for right in built.iter().skip(index + 1) {
            if let (Some(left), Some(right)) = (
                left.runtime
                    .as_ref()
                    .and_then(|runtime| runtime.auth_binding()),
                right
                    .runtime
                    .as_ref()
                    .and_then(|runtime| runtime.auth_binding()),
            ) && left.credential_id() == right.credential_id()
                && !left.same_runtime_as(right)
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
    Ok(())
}

fn validate_provider_input(
    config: &RuntimeMountConfig,
    bytes: &[u8],
    manifest: &ProviderManifest,
) -> Result<(), RegistryError> {
    if ProviderId::from_wasm_bytes(bytes) != config.provider.id {
        return Err(RegistryError::RuntimeError(format!(
            "provider bytes for mount {} do not match {}",
            config.name, config.provider.id
        )));
    }
    if manifest.id != config.provider.meta.name.as_str()
        || manifest.version.as_deref()
            != config
                .provider
                .meta
                .version
                .as_ref()
                .map(omnifs_core::ProviderVersion::as_str)
    {
        return Err(RegistryError::RuntimeError(format!(
            "provider metadata for mount {} does not match its pin",
            config.name
        )));
    }
    Ok(())
}

fn generation_cache_source(
    canonical: &[u8],
    credential_generation: Option<CredentialGeneration>,
) -> Arc<[u8]> {
    const DOMAIN: &[u8] = b"omnifs.projection-generation.v1\0";
    let mut source = Vec::with_capacity(DOMAIN.len() + canonical.len() + 9);
    source.extend_from_slice(DOMAIN);
    source.extend_from_slice(canonical);
    match credential_generation {
        Some(generation) => {
            source.push(1);
            source.extend_from_slice(&generation.get().to_le_bytes());
        },
        None => source.push(0),
    }
    Arc::from(source.into_boxed_slice())
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("config error: {0}")]
    ConfigError(String),
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
    use super::{
        MountBuildInput, MountBuildState, MountTable, ProviderBuildInput, RegistryError,
        generation_cache_source,
    };
    use crate::runtime::host::HostOnline;
    use crate::{
        DirCursor, EngineNamespace, Namespace, NsError, PreparedGeneration, RuntimeMountConfig,
        ServingCell,
    };
    use omnifs_core::{CredentialGeneration, ProviderId, ResourceName};
    use omnifs_provider::{Artifact, ProviderManifest};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    fn wasm_artifact_path(file_name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("host crate must have a workspace parent")
            .parent()
            .expect("workspace root must exist")
            .join("target")
            .join("wasm32-wasip2")
            .join("release")
            .join(file_name)
    }

    fn test_provider_wasm_path() -> PathBuf {
        wasm_artifact_path("test_provider.wasm")
    }

    fn test_host(root: &Path) -> HostOnline {
        crate::test_support::open_test_host(root.join("cache"), root.join("clones"))
            .expect("open test host")
    }

    fn input(
        bytes: &[u8],
        manifest: ProviderManifest,
        name: &str,
        config: serde_json::Value,
        state: MountBuildState,
    ) -> MountBuildInput {
        let artifact = Artifact::from_bytes(format!("{name}.wasm"), bytes.to_vec())
            .expect("parse provider artifact");
        MountBuildInput {
            config: RuntimeMountConfig {
                name: ResourceName::new(name).expect("mount name"),
                provider: artifact.reference(),
                config,
                max_fetch_blob_bytes: None,
            },
            canonical: Arc::from(b"canonical mount".as_slice()),
            provider: Some(ProviderBuildInput {
                bytes: Arc::from(bytes.to_vec().into_boxed_slice()),
                manifest,
            }),
            state,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn durable_input_rejects_invalid_provider_config() {
        let root = tempfile::tempdir().expect("temp root");
        let path = test_provider_wasm_path();
        assert!(path.exists(), "build providers before the host fixture");
        let bytes = std::fs::read(path).expect("read test provider");
        let (_, manifest) = Artifact::from_bytes_with_manifest("test-provider.wasm", bytes.clone())
            .expect("validate test provider");
        let result = MountTable::prepare_durable(
            &test_host(root.path()),
            vec![input(
                &bytes,
                manifest,
                "test",
                serde_json::json!({"unexpected": true}),
                MountBuildState::Active {
                    auth: None,
                    credential_generation: None,
                },
            )],
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
    async fn durable_input_builds_before_timer_activation() {
        let root = tempfile::tempdir().expect("temp root");
        let path = test_provider_wasm_path();
        assert!(path.exists(), "build providers before the host fixture");
        let bytes = std::fs::read(path).expect("read test provider");
        let (_, manifest) = Artifact::from_bytes_with_manifest("test-provider.wasm", bytes.clone())
            .expect("validate test provider");
        let table = Arc::new(
            MountTable::prepare_durable(
                &test_host(root.path()),
                vec![input(
                    &bytes,
                    manifest,
                    "test",
                    serde_json::json!({}),
                    MountBuildState::Active {
                        auth: None,
                        credential_generation: None,
                    },
                )],
            )
            .expect("prepare durable mount"),
        );
        assert!(!table.timers_active.load(Ordering::Acquire));
        assert!(table.get("test").is_some());
        let prepared = PreparedGeneration::new(
            Arc::clone(&table),
            tokio::runtime::Handle::current(),
            crate::GenerationProvenance::default(),
        );
        assert!(!prepared.background_active());
        let _cell = ServingCell::new([0x42; 16], prepared.activate());
        assert!(table.timers_active.load(Ordering::Acquire));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn auth_required_mount_keeps_its_root_but_denies_children() {
        let root = tempfile::tempdir().expect("temp root");
        let path = test_provider_wasm_path();
        assert!(path.exists(), "build providers before the host fixture");
        let bytes = std::fs::read(path).expect("read test provider");
        let (_, manifest) = Artifact::from_bytes_with_manifest("test-provider.wasm", bytes.clone())
            .expect("validate test provider");
        let table = Arc::new(
            MountTable::prepare_durable(
                &test_host(root.path()),
                vec![input(
                    &bytes,
                    manifest,
                    "test",
                    serde_json::json!({}),
                    MountBuildState::AuthRequired,
                )],
            )
            .expect("prepare auth-required mount"),
        );
        let namespace = EngineNamespace::online(table, tokio::runtime::Handle::current());
        let mount = namespace
            .lookup(omnifs_core::path::Path::root(), "test")
            .await
            .expect("configured mount root");
        assert_eq!(
            namespace
                .readdir(mount.path, DirCursor::start(), 0)
                .await
                .unwrap_err(),
            NsError::AuthRequired
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unavailable_provider_keeps_mount_root_without_starting_runtime() {
        let root = tempfile::tempdir().expect("temp root");
        let path = test_provider_wasm_path();
        assert!(path.exists(), "build providers before the host fixture");
        let bytes = std::fs::read(path).expect("read test provider");
        let (_, manifest) = Artifact::from_bytes_with_manifest("test-provider.wasm", bytes.clone())
            .expect("validate test provider");
        let table = Arc::new(
            MountTable::prepare_durable(
                &test_host(root.path()),
                vec![input(
                    &bytes,
                    manifest,
                    "test",
                    serde_json::json!({}),
                    MountBuildState::ProviderUnavailable,
                )],
            )
            .expect("prepare unavailable mount"),
        );
        assert!(table.get("test").is_none());
        let namespace = EngineNamespace::online(table, tokio::runtime::Handle::current());
        let mount = namespace
            .lookup(omnifs_core::path::Path::root(), "test")
            .await
            .expect("configured mount root");
        assert!(matches!(
            namespace.readdir(mount.path, DirCursor::start(), 0).await,
            Err(NsError::Internal { message }) if message.contains("provider is unavailable")
        ));
    }

    #[test]
    fn credential_generation_partitions_projection_identity() {
        let canonical = b"same canonical mount";
        let first = generation_cache_source(canonical, Some(CredentialGeneration::initial()));
        let second = generation_cache_source(
            canonical,
            Some(CredentialGeneration::initial().next().unwrap()),
        );
        assert_ne!(
            super::ProjectionId::new(&first, ProviderId::from_wasm_bytes(b"provider")),
            super::ProjectionId::new(&second, ProviderId::from_wasm_bytes(b"provider"))
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn durable_input_publishes_all_mounts_and_joins_timers() {
        let root = tempfile::tempdir().expect("temp root");
        let path = test_provider_wasm_path();
        if !path.exists() {
            return;
        }
        let bytes = std::fs::read(path).expect("read test provider");
        let (_, manifest) = Artifact::from_bytes_with_manifest("test-provider.wasm", bytes.clone())
            .expect("validate test provider");
        let table = MountTable::prepare_durable(
            &test_host(root.path()),
            vec![input(
                &bytes,
                manifest,
                "test",
                serde_json::json!({}),
                MountBuildState::Active {
                    auth: None,
                    credential_generation: None,
                },
            )],
        )
        .expect("prepare durable mount");
        assert_eq!(table.mounts(), ["test"]);
        table.activate_timers(&tokio::runtime::Handle::current());
        assert_eq!(table.timer_tasks.lock().len(), 1);
        table.begin_retirement();
        table
            .shutdown_all_joined(tokio::time::Instant::now() + Duration::from_secs(1))
            .await;
        assert!(table.timer_tasks.lock().is_empty());
    }
}
