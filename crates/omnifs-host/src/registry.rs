//! Provider registry: loading and lifecycle management for WASM providers.
//!
//! Scans the providers directory, instantiates providers via `Runtime`,
//! and manages timer-driven refresh tasks.

use crate::cloner::GitCloner;
use crate::mounts::{Catalog, Resolved, Spec};
use crate::tools::archive::{ARCHIVE_TOOL_WASM, ArchiveExtractorComponent, DEFAULT_LIMITS};
use crate::{Artifact, BuildError, Dirs, Runtime, component_engine};
use omnifs_cache::Caches;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tracing::{debug, info, warn};

/// Registry of loaded WASM providers.
///
/// Scans configuration directories, instantiates providers, and manages
/// their lifecycle including timer-driven refresh tasks.
pub struct ProviderRegistry {
    instances: HashMap<String, Arc<Runtime>>,
    root_mount: Option<String>,
    timer_shutdown: watch::Sender<bool>,
    timer_tasks: parking_lot::Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl ProviderRegistry {
    #[doc(hidden)]
    pub fn empty_for_test() -> Self {
        let (timer_shutdown, _) = watch::channel(false);
        Self {
            instances: HashMap::new(),
            root_mount: None,
            timer_shutdown,
            timer_tasks: parking_lot::Mutex::new(Vec::new()),
        }
    }

    pub fn load(dirs: Dirs<'_>, cloner: &Arc<GitCloner>) -> Result<Self, RegistryError> {
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

        // Global cache handles: one durable object.redb and one disposable
        // view.redb deleted + recreated on startup (Codex #5). Shared across
        // all provider runtimes; per-mount isolation via key prefix.
        let caches = Caches::open(dirs.cache_dir)
            .map_err(|e| RegistryError::RuntimeError(format!("cache open: {e}")))?;

        let mut instances = HashMap::new();
        let mut root_mount: Option<String> = None;
        let mount_catalog = Catalog::new(dirs.mounts_dir, dirs.providers_dir);

        let config_paths = dirs
            .mount_config_paths()
            .map_err(RegistryError::ScanFailed)?;
        for path in config_paths {
            match Self::load_mount_runtime(
                &engine,
                &path,
                dirs,
                cloner,
                &extractor,
                &mount_catalog,
                &caches,
            ) {
                Ok((mount, is_root, runtime)) => {
                    if instances.contains_key(&mount) {
                        warn!(
                            mount = mount,
                            file = %path.display(),
                            "duplicate mount name; skipping provider (already loaded from another file)"
                        );
                        continue;
                    }
                    if is_root {
                        if let Some(existing) = &root_mount {
                            warn!(
                                mount = mount,
                                existing = existing.as_str(),
                                "multiple root_mount providers; ignoring root_mount for this one"
                            );
                        } else {
                            root_mount = Some(mount.clone());
                        }
                    }
                    info!(mount = mount, file = %path.display(), root = is_root, "loaded provider");
                    instances.insert(mount, Arc::new(runtime));
                },
                Err(e @ RegistryError::ConfigError(_)) => return Err(e),
                Err(e) => {
                    warn!(file = %path.display(), error = %e, "skipping provider");
                },
            }
        }

        let (timer_shutdown, _) = watch::channel(false);
        Ok(Self {
            instances,
            root_mount,
            timer_shutdown,
            timer_tasks: parking_lot::Mutex::new(Vec::new()),
        })
    }

    fn load_mount_runtime(
        engine: &wasmtime::Engine,
        config_path: &Path,
        dirs: Dirs<'_>,
        cloner: &Arc<GitCloner>,
        extractor: &Arc<ArchiveExtractorComponent>,
        mount_catalog: &Catalog,
        caches: &Arc<omnifs_cache::Caches>,
    ) -> Result<(String, bool, Runtime), RegistryError> {
        let config = mount_catalog
            .load_spec(config_path)
            .map_err(|error| RegistryError::ConfigError(error.to_string()))?;

        let wasm_path = dirs.provider_path(&config.provider);
        if !wasm_path.exists() {
            return Err(RegistryError::ProviderNotFound(
                wasm_path.display().to_string(),
            ));
        }
        let resolved = resolve_mount_for_wasm(&wasm_path, config).map_err(|e| match e {
            BuildError::InvalidConfig(message) => RegistryError::ConfigError(format!(
                "config file {}: {message}",
                config_path.display()
            )),
            other => RegistryError::RuntimeError(other.to_string()),
        })?;
        let is_root = resolved.root_mount;
        let mount = resolved.mount.clone();
        let runtime = Runtime::new(
            engine,
            &wasm_path,
            &resolved,
            cloner.clone(),
            dirs,
            extractor.clone(),
            caches,
        )
        .map_err(|e| match e {
            BuildError::InvalidConfig(message) => RegistryError::ConfigError(format!(
                "config file {}: {message}",
                config_path.display()
            )),
            other => RegistryError::RuntimeError(other.to_string()),
        })?;

        Ok((mount, is_root, runtime))
    }

    pub fn get(&self, mount: &str) -> Option<&Arc<Runtime>> {
        self.instances.get(mount)
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

    /// Returns the mount name of the root-mounted provider, if any.
    pub fn root_mount_name(&self) -> Option<&str> {
        self.root_mount.as_deref()
    }

    pub fn shutdown_all(&self) {
        let _ = self.timer_shutdown.send(true);
        for task in self.timer_tasks.lock().drain(..) {
            task.abort();
        }
        for (mount, runtime) in &self.instances {
            if let Err(e) = runtime.shutdown() {
                warn!(mount, error = %e, "shutdown failed");
            }
        }
    }

    pub fn start_timers(&self, handle: &tokio::runtime::Handle) {
        let mut tasks = self.timer_tasks.lock();
        if !tasks.is_empty() {
            return;
        }

        for (mount, runtime) in &self.instances {
            let interval_secs = runtime.requested_capabilities().refresh_interval_secs;
            if interval_secs == 0 {
                continue;
            }

            let mount = mount.clone();
            let runtime = runtime.clone();
            let mut shutdown = self.timer_shutdown.subscribe();
            tasks.push(handle.spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_secs(u64::from(interval_secs)));
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
            }));
        }
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
    #[error("failed to scan providers directory: {0}")]
    ScanFailed(std::io::Error),
    #[error("config error: {0}")]
    ConfigError(String),
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

    #[test]
    fn load_fails_on_invalid_provider_config() {
        // The test provider's embedded manifest declares a configSchema
        // (additionalProperties: false, no properties), so the host's
        // validate_instance_config rejects a mount config with extra fields.
        let config_dir = tempfile::tempdir().expect("temp config dir");
        let cache_dir = tempfile::tempdir().expect("temp cache dir");
        let providers_dir = tempfile::tempdir().expect("temp providers dir");
        let mounts_dir = config_dir.path().join("mounts");
        std::fs::create_dir_all(&mounts_dir).expect("create mounts dir");

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

        std::fs::write(
            mounts_dir.join("invalid.json"),
            r#"{
                "provider": "test_provider.wasm",
                "mount": "test",
                "config": {
                    "unexpected": true
                }
            }"#,
        )
        .expect("write provider config");

        let cloner = Arc::new(GitCloner::new(cache_dir.path().join("clones")));
        match ProviderRegistry::load(
            Dirs::new(
                cache_dir.path(),
                config_dir.path(),
                &mounts_dir,
                providers_dir.path(),
            ),
            &cloner,
        ) {
            Err(RegistryError::ConfigError(message)) => {
                assert!(message.contains("failed validation"));
                assert!(message.contains("invalid.json"));
                assert!(message.contains("mount test"));
            },
            Err(other) => panic!("expected config error, got {other}"),
            Ok(_) => panic!("expected invalid provider config to abort load"),
        }
    }
}
