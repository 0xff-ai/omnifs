//! Shared Wasmtime component-engine setup.

use std::path::Path;

use omnifs_workspace::ids::ProviderId;
use omnifs_workspace::provider::ProviderStore;
use wasmtime::component::Component;
use wasmtime::{Cache, CacheConfig, Config, Engine};

/// Build a Wasmtime engine configured for the component model.
///
/// `cache_dir`, when `Some`, is where compiled component artifacts are
/// persisted so engine creation doesn't re-codegen identical wasm on every
/// run; the host points this at `<cache>/wasm` so the artifacts live with the
/// rest of its state rather than in a global per-user directory. `None`
/// disables the on-disk cache (tests that don't want a shared artifact dir).
///
/// Callers receive a chance to apply lifecycle-specific options, such as the
/// provider compiler strategy.
pub fn component_engine(
    cache_dir: Option<&Path>,
    configure: impl FnOnce(&mut Config),
) -> wasmtime::Result<Engine> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    config.wasm_component_model_async(true);
    config.wasm_component_model_more_async_builtins(true);
    config.wasm_component_model_async_stackful(true);
    config.concurrency_support(true);
    // Persist compiled component artifacts under the host cache so engine
    // creation doesn't re-codegen identical wasm on every run. Silently
    // degrade if the cache can't be initialised (read-only dir, locked-down
    // host): the compile still works, just uncached. `Cache::new` creates the
    // directory itself.
    if let Some(dir) = cache_dir {
        let mut cache_config = CacheConfig::new();
        cache_config.with_directory(dir);
        if let Ok(cache) = Cache::new(cache_config) {
            config.cache(Some(cache));
        }
    }
    configure(&mut config);
    Engine::new(&config)
}

/// Compiles exact retained provider components into the same Wasmtime cache
/// used by the daemon, without instantiating or executing provider code.
#[derive(Clone)]
pub struct ComponentCompiler {
    engine: Engine,
    providers: ProviderStore,
}

impl ComponentCompiler {
    pub fn new(cache_dir: &Path, providers_dir: &Path) -> wasmtime::Result<Self> {
        Ok(Self {
            engine: component_engine(Some(cache_dir), |_| {})?,
            providers: ProviderStore::new(providers_dir),
        })
    }

    pub fn prepare(&self, id: &ProviderId) -> wasmtime::Result<()> {
        Component::from_file(&self.engine, self.providers.artifact_path(id)).map(|_| ())
    }
}
