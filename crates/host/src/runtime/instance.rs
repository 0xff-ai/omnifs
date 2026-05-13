//! `ProviderInstance` — the Wasmtime mechanics boundary.
//!
//! Owns the wasm store, the generated bindings, and the serialized
//! provider config. Every method takes a fresh store lock per call.
//! `ProviderRuntime` composes this with orchestration concerns
//! (executors, caches, activity, invalidation, inflight).

use std::path::Path;

use parking_lot::Mutex;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime_wasi::WasiCtxBuilder;

use crate::Provider;
use crate::omnifs::provider::types as wit_types;
use crate::runtime::wasm;

use super::{HostState, Op, RuntimeBuildError, RuntimeError};

/// Owns the WASM component instance and serialized provider config.
///
/// All Wasmtime store access goes through this type. Methods acquire
/// the store lock on entry and release it on return; no caller holds
/// the store across host-side work.
pub struct ProviderInstance {
    store: Mutex<wasmtime::Store<HostState>>,
    bindings: Provider,
    config_bytes: Vec<u8>,
}

impl ProviderInstance {
    pub fn new(
        engine: &wasmtime::Engine,
        wasm_path: &Path,
        config_bytes: Vec<u8>,
    ) -> std::result::Result<Self, RuntimeBuildError> {
        let mut linker = Linker::<HostState>::new(engine);
        wasm::add_wasi_to_linker::<HostState>(&mut linker)?;
        Provider::add_to_linker::<HostState, HostState>(&mut linker, |state| state)?;

        let component = Component::from_file(engine, wasm_path)?;
        let wasi = WasiCtxBuilder::new().build();
        let mut store = wasmtime::Store::new(
            engine,
            HostState {
                wasi,
                table: ResourceTable::new(),
            },
        );

        let bindings = Provider::instantiate(&mut store, &component, &linker)?;

        Ok(Self {
            store: Mutex::new(store),
            bindings,
            config_bytes,
        })
    }

    pub fn start_op(
        &self,
        op: &Op,
        id: u64,
    ) -> std::result::Result<wit_types::ProviderStep, RuntimeError> {
        let mut store = self.store.lock();
        let browse = self.bindings.omnifs_provider_browse();
        match op {
            Op::LookupChild { parent_path, name } => browse
                .call_lookup_child(&mut *store, id, parent_path, name)
                .map_err(Into::into),
            Op::ListChildren { path } => browse
                .call_list_children(&mut *store, id, path)
                .map_err(Into::into),
            Op::ReadFile { path } => browse
                .call_read_file(&mut *store, id, path)
                .map_err(Into::into),
            Op::OpenFile { path } => browse
                .call_open_file(&mut *store, id, path)
                .map_err(Into::into),
            Op::ReadChunk {
                handle,
                offset,
                length,
            } => browse
                .call_read_chunk(&mut *store, id, *handle, *offset, *length)
                .map_err(Into::into),
            Op::OnEvent { event } => self
                .bindings
                .omnifs_provider_notify()
                .call_on_event(&mut *store, id, event)
                .map_err(Into::into),
            Op::Initialize => unreachable!(
                "Op::Initialize is driven directly through ProviderInstance::initialize; \
                 start_op never receives it"
            ),
        }
    }

    #[allow(clippy::needless_pass_by_value)] // generated WIT binding requires &Vec
    pub fn resume(
        &self,
        id: u64,
        results: Vec<wit_types::CalloutResult>,
    ) -> std::result::Result<wit_types::ProviderStep, RuntimeError> {
        let mut store = self.store.lock();
        Ok(self
            .bindings
            .omnifs_provider_continuation()
            .call_resume(&mut *store, id, &results)?)
    }

    pub fn initialize(&self) -> std::result::Result<wit_types::ProviderReturn, RuntimeError> {
        let mut store = self.store.lock();
        Ok(self
            .bindings
            .omnifs_provider_lifecycle()
            .call_initialize(&mut *store, &self.config_bytes)?)
    }

    pub fn shutdown(&self) -> std::result::Result<(), RuntimeError> {
        let mut store = self.store.lock();
        self.bindings
            .omnifs_provider_lifecycle()
            .call_shutdown(&mut *store)?;
        Ok(())
    }

    pub fn config_schema(&self) -> std::result::Result<Option<String>, RuntimeError> {
        let mut store = self.store.lock();
        Ok(self
            .bindings
            .omnifs_provider_lifecycle()
            .call_get_config_schema(&mut *store)?)
    }

    pub fn capabilities(
        &self,
    ) -> std::result::Result<wit_types::RequestedCapabilities, RuntimeError> {
        let mut store = self.store.lock();
        Ok(self
            .bindings
            .omnifs_provider_lifecycle()
            .call_capabilities(&mut *store)?)
    }

    pub fn close_file(&self, handle: u64) -> std::result::Result<(), RuntimeError> {
        let mut store = self.store.lock();
        self.bindings
            .omnifs_provider_browse()
            .call_close_file(&mut *store, handle)?;
        Ok(())
    }
}
