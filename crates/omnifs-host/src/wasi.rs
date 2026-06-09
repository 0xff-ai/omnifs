//! WASI store state and host-interface implementations.
//!
//! This module exists so runtime.rs does not mix WASI plumbing with
//! engine/instance/mount lifecycle code.

use omnifs_wit::provider::log::Host as LogHost;
use omnifs_wit::provider::types::{self as wit_types, Host as TypesHost};
use tracing::{debug, error, info, trace, warn};
use wasmtime::component::HasData;
use wasmtime_wasi::{ResourceTable, WasiCtx, WasiCtxView, WasiView};

pub(crate) struct HostState {
    pub(crate) wasi: WasiCtx,
    pub(crate) table: ResourceTable,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl HasData for HostState {
    type Data<'a> = &'a mut HostState;
}

impl TypesHost for HostState {}
impl LogHost for HostState {
    fn log(&mut self, entry: wit_types::LogEntry) {
        match entry.level {
            wit_types::LogLevel::Trace => trace!("{}", entry.message),
            wit_types::LogLevel::Debug => debug!("{}", entry.message),
            wit_types::LogLevel::Info => info!("{}", entry.message),
            wit_types::LogLevel::Warn => warn!("{}", entry.message),
            wit_types::LogLevel::Error => error!("{}", entry.message),
        }
    }
}
