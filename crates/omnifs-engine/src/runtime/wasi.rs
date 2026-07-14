//! WASI store state and host-interface implementations.
//!
//! This module exists so runtime.rs does not mix WASI plumbing with
//! engine/instance/mount lifecycle code.

use crate::callouts::{CalloutHost, callout_internal};
use omnifs_wit::provider::log::Host as LogHost;
use omnifs_wit::provider::omnifs::provider::callouts::{
    Host as CalloutsHost, HostWithStore as CalloutsHostWithStore,
};
use omnifs_wit::provider::types::{self as wit_types, Host as TypesHost};
use tracing::{debug, error, info, trace, warn};
use wasmtime::component::{Accessor, HasData};
use wasmtime_wasi::{ResourceTable, WasiCtx, WasiCtxView, WasiView};

pub(crate) struct HostState {
    pub(crate) wasi: WasiCtx,
    pub(crate) table: ResourceTable,
    pub(crate) callouts: Option<CalloutHost>,
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

impl HostState {
    async fn dispatch_callout<T>(
        accessor: &Accessor<T, Self>,
        id: u64,
        callout: wit_types::Callout,
    ) -> wit_types::CalloutResult {
        let callouts = accessor.with(|mut access| access.get().callouts.clone());
        match callouts {
            Some(callouts) => callouts.dispatch(id, callout).await,
            None => callout_internal("provider callouts are not initialized"),
        }
    }
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

impl<T: Send + 'static> CalloutsHostWithStore<T> for HostState {
    async fn fetch(
        accessor: &Accessor<T, Self>,
        id: u64,
        req: wit_types::HttpRequest,
    ) -> wit_types::CalloutResult {
        Self::dispatch_callout(accessor, id, wit_types::Callout::Fetch(req)).await
    }

    async fn git_open_repo(
        accessor: &Accessor<T, Self>,
        id: u64,
        req: wit_types::GitOpenRequest,
    ) -> wit_types::CalloutResult {
        Self::dispatch_callout(accessor, id, wit_types::Callout::GitOpenRepo(req)).await
    }

    async fn fetch_blob(
        accessor: &Accessor<T, Self>,
        id: u64,
        req: wit_types::BlobFetchRequest,
    ) -> wit_types::CalloutResult {
        Self::dispatch_callout(accessor, id, wit_types::Callout::FetchBlob(req)).await
    }

    async fn read_blob(
        accessor: &Accessor<T, Self>,
        id: u64,
        req: wit_types::ReadBlobRequest,
    ) -> wit_types::CalloutResult {
        Self::dispatch_callout(accessor, id, wit_types::Callout::ReadBlob(req)).await
    }
}

impl CalloutsHost for HostState {}
