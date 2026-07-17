//! Shared Wasmtime component-engine ownership.

use std::path::Path;
use std::pin::Pin;

use futures::{Stream, StreamExt as _, stream};
use omnifs_workspace::ids::ProviderId;
use omnifs_workspace::provider::Provider;
use wasmtime::component::Component;
use wasmtime::{Cache, CacheConfig, Config, Engine};

const MAX_WARM_PARALLELISM: usize = 4;

/// The production Wasmtime engine used to load provider components.
///
/// Serving and provider warmup share this owner so both paths use exactly the
/// same component-model configuration and cache key.
#[derive(Clone)]
pub struct ComponentEngine {
    inner: Engine,
}

impl ComponentEngine {
    /// Create the production component engine.
    ///
    /// `cache_dir`, when present, stores Wasmtime's compiled artifacts with
    /// the workspace. An unavailable cache degrades to uncached compilation.
    pub fn new(cache_dir: Option<&Path>) -> wasmtime::Result<Self> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        config.wasm_component_model_async(true);
        config.wasm_component_model_more_async_builtins(true);
        config.wasm_component_model_async_stackful(true);
        config.concurrency_support(true);
        if let Some(dir) = cache_dir {
            let mut cache_config = CacheConfig::new();
            cache_config.with_directory(dir);
            if let Ok(cache) = Cache::new(cache_config) {
                config.cache(Some(cache));
            }
        }
        Ok(Self {
            inner: Engine::new(&config)?,
        })
    }

    /// Load one provider component through the production engine.
    pub fn load(&self, wasm_path: &Path) -> wasmtime::Result<Component> {
        Component::from_file(&self.inner, wasm_path)
    }

    /// Warm exact retained providers with bounded compilation concurrency.
    ///
    /// Each outcome is independent so the caller can persist aggregate
    /// progress while the stream is consumed.
    pub fn warm(
        &self,
        providers: Vec<Provider>,
    ) -> Pin<Box<dyn Stream<Item = WarmOutcome> + Send>> {
        let parallelism = std::thread::available_parallelism()
            .map_or(1, std::num::NonZeroUsize::get)
            .min(MAX_WARM_PARALLELISM)
            .min(providers.len().max(1));
        let engine = self.clone();
        let jobs = stream::iter(providers.into_iter().map(move |provider| {
            let engine = engine.clone();
            async move {
                let provider_id = provider.id;
                let wasm_path = provider.wasm_path().to_path_buf();
                let result = tokio::task::spawn_blocking(move || engine.load(&wasm_path))
                    .await
                    .map_err(|error| {
                        wasmtime::Error::msg(format!("component loader task failed: {error}"))
                    })
                    .and_then(|result| result)
                    .map(|_| ());
                WarmOutcome {
                    provider_id,
                    result,
                }
            }
        }))
        .buffer_unordered(parallelism);
        Box::pin(jobs)
    }

    pub(crate) fn inner(&self) -> &Engine {
        &self.inner
    }
}

/// The result of warming one exact provider.
pub struct WarmOutcome {
    pub provider_id: ProviderId,
    pub result: wasmtime::Result<()>,
}
