//! Shared Wasmtime component-engine ownership.

use std::path::Path;

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

    pub(crate) fn inner(&self) -> &Engine {
        &self.inner
    }
}
