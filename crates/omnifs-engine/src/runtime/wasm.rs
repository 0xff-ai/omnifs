//! Shared Wasmtime component-engine ownership.

use std::path::Path;

use omnifs_core::ProviderId;
use wasmtime::component::Component;
use wasmtime::{Cache, CacheConfig, Config, Engine};

/// The production Wasmtime engine used to load provider components.
#[derive(Clone)]
pub struct ComponentEngine {
    inner: Engine,
}

impl ComponentEngine {
    /// Create the production component engine.
    ///
    /// `cache_dir` stores Wasmtime's compiled artifacts with the daemon state.
    /// Cache initialization failure prevents engine creation.
    pub fn new(cache_dir: &Path) -> wasmtime::Result<Self> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        config.wasm_component_model_async(true);
        config.wasm_component_model_more_async_builtins(true);
        config.wasm_component_model_async_stackful(true);
        config.concurrency_support(true);
        let mut cache_config = CacheConfig::new();
        cache_config.with_directory(cache_dir);
        config.cache(Some(Cache::new(cache_config)?));
        Ok(Self {
            inner: Engine::new(&config)?,
        })
    }

    /// Load one provider component through the production engine.
    pub fn load(&self, component_bytes: &[u8]) -> wasmtime::Result<Component> {
        Component::new(&self.inner, component_bytes)
    }

    /// Compile one exact provider into the required durable Wasmtime cache.
    ///
    /// This operation intentionally does not return or retain the compiled
    /// component. Daemon reconciliation calls [`Self::load`] later for only
    /// the providers used by the active generation.
    pub fn prepare(&self, provider_id: ProviderId, component_bytes: &[u8]) -> wasmtime::Result<()> {
        let actual = ProviderId::from_wasm_bytes(component_bytes);
        if actual != provider_id {
            return Err(wasmtime::Error::msg(format!(
                "provider digest mismatch: expected {provider_id}, computed {actual}"
            )));
        }
        drop(Component::new(&self.inner, component_bytes)?);
        Ok(())
    }

    pub(crate) fn inner(&self) -> &Engine {
        &self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn prepared_component_is_available_to_a_fresh_cached_engine() {
        let cache = tempfile::tempdir().expect("create private Wasmtime cache");
        let cache_dir = cache.path().join("compiled");
        fs::create_dir(&cache_dir).expect("create cache directory");
        let wasm_path = omnifs_itest::provider_wasm_path("test_provider.wasm");
        let bytes = fs::read(&wasm_path).expect("read test provider component");
        let provider_id = ProviderId::from_wasm_bytes(&bytes);

        let engine = ComponentEngine::new(&cache_dir).expect("create first cached engine");
        engine
            .prepare(provider_id, &bytes)
            .expect("prepare provider component");
        drop(engine);

        assert!(
            contains_file(&cache_dir),
            "preparation must populate the private Wasmtime cache"
        );

        let fresh = ComponentEngine::new(&cache_dir).expect("create fresh cached engine");
        let component = fresh
            .load(&bytes)
            .expect("load prepared component with fresh engine");
        drop(component);
    }

    #[test]
    fn prepare_rejects_bytes_for_another_digest() {
        let cache = tempfile::tempdir().expect("create private Wasmtime cache");
        let engine = ComponentEngine::new(cache.path()).expect("create cached engine");
        let bytes = fs::read(omnifs_itest::provider_wasm_path("test_provider.wasm"))
            .expect("read test provider component");

        let error = engine
            .prepare(ProviderId::from_wasm_bytes(b"different"), &bytes)
            .expect_err("digest mismatch must fail");
        assert!(error.to_string().contains("provider digest mismatch"));
    }

    fn contains_file(path: &Path) -> bool {
        let Ok(entries) = fs::read_dir(path) else {
            return false;
        };
        entries.filter_map(Result::ok).any(|entry| {
            entry
                .file_type()
                .is_ok_and(|kind| kind.is_file() || kind.is_dir() && contains_file(&entry.path()))
        })
    }
}
