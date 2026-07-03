//! Shared Wasmtime component-host primitives.
//!
//! The provider runtime needs lower-level Wasm setup: component-model
//! engines, a WASI linker, and consistent error messages.

use std::path::Path;

use wasmtime::{Cache, CacheConfig, Config, Engine, Strategy};
use wasmtime_wasi::WasiView;

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

/// Compiler strategy for the *provider* engine, read from
/// `OMNIFS_WASM_COMPILER`: `winch` selects the single-pass baseline compiler
/// (fast compile, slower execution), `cranelift` forces the optimizing
/// compiler, and unset leaves the wasmtime default (Cranelift). Returns `None`
/// when no override applies so the caller leaves `Config` untouched.
///
/// Providers suspend on host callouts rather than running hot loops, so Winch's
/// execution penalty barely touches them while its compile speed unblocks cold
/// startup.
pub fn provider_compiler_strategy() -> Option<Strategy> {
    let raw = std::env::var("OMNIFS_WASM_COMPILER").ok()?;
    match raw.trim().to_ascii_lowercase().as_str() {
        "" => None,
        "winch" => Some(Strategy::Winch),
        "cranelift" => Some(Strategy::Cranelift),
        other => {
            tracing::warn!(
                value = other,
                "ignoring unrecognized OMNIFS_WASM_COMPILER (expected 'winch' or 'cranelift')"
            );
            None
        },
    }
}

/// Add async WASI Preview 2 imports to a component linker.
pub(crate) fn add_wasi_to_linker<T>(
    linker: &mut wasmtime::component::Linker<T>,
) -> wasmtime::Result<()>
where
    T: WasiView + 'static,
{
    wasmtime_wasi::p2::add_to_linker_async::<T>(linker)
}
