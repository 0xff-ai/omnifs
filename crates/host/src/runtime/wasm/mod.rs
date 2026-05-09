//! Shared Wasmtime component-host primitives.
//!
//! Providers and embedded sandboxed tools have different lifecycles, but
//! they both need the same lower-level Wasm setup: component-model
//! engines, WASI linkers, store limits, and consistent error messages.

use wasmtime::{Config, Engine, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::WasiView;

/// Build a Wasmtime engine configured for the component model.
///
/// Callers receive a chance to apply lifecycle-specific options. The
/// provider runtime uses the base component configuration, while
/// one-shot sandboxed tools enable fuel consumption on top.
pub(crate) fn component_engine(configure: impl FnOnce(&mut Config)) -> wasmtime::Result<Engine> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    configure(&mut config);
    Engine::new(&config)
}

/// Add synchronous WASI Preview 2 imports to a component linker.
pub(crate) fn add_wasi_to_linker<T>(
    linker: &mut wasmtime::component::Linker<T>,
) -> wasmtime::Result<()>
where
    T: WasiView + 'static,
{
    wasmtime_wasi::p2::add_to_linker_sync::<T>(linker)
}

/// Build store limits for a sandboxed component invocation.
pub(crate) fn store_limits(max_memory_bytes: usize) -> StoreLimits {
    StoreLimitsBuilder::new()
        .memory_size(max_memory_bytes)
        .build()
}
