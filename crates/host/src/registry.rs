//! Provider registry: loading and lifecycle management for WASM providers.
//!
//! Scans the providers directory, instantiates providers via `ProviderRuntime`,
//! and manages timer-driven refresh tasks.

use crate::config::InstanceConfig;
use crate::runtime::cloner::GitCloner;
use crate::runtime::tools::archive::{ArchiveExtractorComponent, DEFAULT_LIMITS};
use crate::runtime::wasm;
use crate::runtime::{ProviderRuntime, RuntimeError};
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
    #[allow(dead_code)] // stored for future use (hot-reloading providers)
    engine: wasmtime::Engine,
    instances: HashMap<String, Arc<ProviderRuntime>>,
    root_mount: Option<String>,
    timer_shutdown: watch::Sender<bool>,
    timer_tasks: parking_lot::Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl ProviderRegistry {
    pub fn load(
        config_dir: &Path,
        plugin_dir: &Path,
        cloner: &Arc<GitCloner>,
        cache_dir: &Path,
    ) -> Result<Self, RegistryError> {
        let engine = wasm::component_engine(|_| {})
            .map_err(|e| RegistryError::RuntimeError(format!("provider engine init: {e}")))?;

        // One extractor (engine + parsed component + linker pre) shared
        // across every mount; the per-call sandbox lives on a fresh
        // `wasmtime::Store`.
        let extractor = Arc::new(
            ArchiveExtractorComponent::new(DEFAULT_LIMITS)
                .map_err(|e| RegistryError::RuntimeError(format!("extractor init: {e}")))?,
        );

        let mut instances = HashMap::new();
        let mut root_mount = None;

        let providers_dir = config_dir.join("providers");
        if !providers_dir.exists() {
            let (timer_shutdown, _) = watch::channel(false);
            return Ok(Self {
                engine,
                instances,
                root_mount,
                timer_shutdown,
                timer_tasks: parking_lot::Mutex::new(Vec::new()),
            });
        }

        for entry in std::fs::read_dir(&providers_dir).map_err(RegistryError::ScanFailed)? {
            let entry = entry.map_err(RegistryError::ScanFailed)?;
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }

            match Self::load_instance(&engine, &path, plugin_dir, cloner, cache_dir, &extractor) {
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
            engine,
            instances,
            root_mount,
            timer_shutdown,
            timer_tasks: parking_lot::Mutex::new(Vec::new()),
        })
    }

    fn load_instance(
        engine: &wasmtime::Engine,
        config_path: &Path,
        plugin_dir: &Path,
        cloner: &Arc<GitCloner>,
        cache_dir: &Path,
        extractor: &Arc<ArchiveExtractorComponent>,
    ) -> Result<(String, bool, ProviderRuntime), RegistryError> {
        let config = InstanceConfig::from_file(config_path)
            .map_err(|e| RegistryError::ConfigError(e.to_string()))?;

        let wasm_path = plugin_dir.join(&config.plugin);
        if !wasm_path.exists() {
            return Err(RegistryError::PluginNotFound(
                wasm_path.display().to_string(),
            ));
        }

        let is_root = config.root_mount;
        let runtime = ProviderRuntime::new(
            engine,
            &wasm_path,
            &config,
            cloner.clone(),
            cache_dir,
            &config.mount,
            extractor.clone(),
        )
        .map_err(|e| match e {
            RuntimeError::InvalidConfig(message) => RegistryError::ConfigError(format!(
                "config file {}: {message}",
                config_path.display()
            )),
            other => RegistryError::RuntimeError(other.to_string()),
        })?;

        Ok((config.mount, is_root, runtime))
    }

    pub fn get(&self, mount: &str) -> Option<&Arc<ProviderRuntime>> {
        self.instances.get(mount)
    }

    pub fn mounts(&self) -> Vec<String> {
        self.instances.keys().cloned().collect()
    }

    pub fn runtime_entries(&self) -> Vec<(String, Arc<ProviderRuntime>)> {
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
            let interval_secs = match runtime.capabilities() {
                Ok(caps) => caps.refresh_interval_secs,
                Err(e) => {
                    warn!(mount, error = %e, "failed to read provider capabilities");
                    continue;
                },
            };
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

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("failed to scan providers directory: {0}")]
    ScanFailed(std::io::Error),
    #[error("config error: {0}")]
    ConfigError(String),
    #[error("plugin not found: {0}")]
    PluginNotFound(String),
    #[error("runtime error: {0}")]
    RuntimeError(String),
}

#[cfg(test)]
mod tests {
    use super::{ProviderRegistry, RegistryError};
    use crate::runtime::cloner::GitCloner;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    fn test_provider_wasm_path() -> PathBuf {
        let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("host crate must have a workspace parent")
            .parent()
            .expect("workspace root must exist");
        workspace_root
            .join("target")
            .join("wasm32-wasip2")
            .join("release")
            .join("test_provider.wasm")
    }

    #[test]
    fn load_fails_on_invalid_provider_config() {
        let config_dir = tempfile::tempdir().expect("temp config dir");
        let cache_dir = tempfile::tempdir().expect("temp cache dir");
        let plugin_dir = tempfile::tempdir().expect("temp plugin dir");
        let providers_dir = config_dir.path().join("providers");
        std::fs::create_dir_all(&providers_dir).expect("create providers dir");

        let provider_wasm = test_provider_wasm_path();
        assert!(
            provider_wasm.exists(),
            "test provider missing at {}. Run `just build-providers` first.",
            provider_wasm.display()
        );
        std::fs::copy(&provider_wasm, plugin_dir.path().join("test_provider.wasm"))
            .expect("copy test provider");

        std::fs::write(
            providers_dir.join("invalid.json"),
            r#"{
                "plugin": "test_provider.wasm",
                "mount": "test",
                "config": {
                    "unexpected": true
                }
            }"#,
        )
        .expect("write provider config");

        let cloner = Arc::new(GitCloner::new(cache_dir.path().join("clones")));
        match ProviderRegistry::load(
            config_dir.path(),
            plugin_dir.path(),
            &cloner,
            cache_dir.path(),
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
