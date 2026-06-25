//! `Instance` — the Wasmtime mechanics boundary.
//!
//! Owns the wasm store, the generated bindings, and the serialized
//! provider config. Every method takes a fresh store lock per call.
//! `Runtime` composes this with orchestration concerns
//! (executors, caches, activity, invalidation, inflight).

use std::path::{Component as PathComponent, Path, PathBuf};

use parking_lot::Mutex;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtxBuilder};

use crate::Provider;
use crate::wasi::HostState;
use crate::wasm;
use crate::{BuildError, Error, Op};
use omnifs_caps::{PreopenMode, PreopenedPath};
use omnifs_wit::provider::types as wit_types;

/// Owns the WASM component instance and serialized provider config.
///
/// All Wasmtime store access goes through this type. Methods acquire
/// the store lock on entry and release it on return; no caller holds
/// the store across host-side work.
pub struct Instance {
    store: Mutex<wasmtime::Store<HostState>>,
    bindings: Provider,
    config_bytes: Vec<u8>,
}

impl Instance {
    pub fn new(
        engine: &wasmtime::Engine,
        wasm_path: &Path,
        config_bytes: Vec<u8>,
        preopens: &[PreopenedPath],
    ) -> std::result::Result<Self, BuildError> {
        let mut linker = Linker::<HostState>::new(engine);
        wasm::add_wasi_to_linker::<HostState>(&mut linker)?;
        Provider::add_to_linker::<HostState, HostState>(&mut linker, |state| state)?;

        let component = Component::from_file(engine, wasm_path)?;
        let wasi = build_wasi_ctx(preopens)?;
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
    ) -> std::result::Result<wit_types::ProviderStep, Error> {
        without_tokio_handle(|| {
            let mut store = self.store.lock();
            let namespace = self.bindings.omnifs_provider_namespace();
            match op {
                Op::LookupChild { parent_path, name } => namespace
                    .call_lookup_child(&mut *store, id, parent_path.as_str(), name.as_str())
                    .map_err(Into::into),
                Op::ListChildren {
                    path,
                    cached_validator,
                    cursor,
                } => namespace
                    .call_list_children(
                        &mut *store,
                        id,
                        path.as_str(),
                        cached_validator.as_deref(),
                        cursor.as_ref(),
                    )
                    .map_err(Into::into),
                Op::ReadFile {
                    path,
                    content_type,
                    cached_canonical,
                } => namespace
                    .call_read_file(
                        &mut *store,
                        id,
                        path.as_str(),
                        content_type,
                        cached_canonical.as_ref(),
                    )
                    .map_err(Into::into),
                Op::OpenFile { path } => namespace
                    .call_open_file(&mut *store, id, path.as_str())
                    .map_err(Into::into),
                Op::ReadChunk {
                    handle,
                    offset,
                    length,
                } => namespace
                    .call_read_chunk(&mut *store, id, *handle, *offset, *length)
                    .map_err(Into::into),
                Op::OnEvent { event } => self
                    .bindings
                    .omnifs_provider_notify()
                    .call_on_event(&mut *store, id, event)
                    .map_err(Into::into),
                // `Op::Initialize` is driven directly through
                // `Instance::initialize`: the WIT lifecycle method
                // returns a `provider-return`, never suspends, and has no
                // correlation id. The variant remains in the `Op` enum so
                // `finish_provider_return` and `Validator` can tag the
                // initialize result with the operation that produced it.
                Op::Initialize => {
                    unreachable!("Op::Initialize never reaches start_op; see Instance::initialize")
                },
            }
        })
    }

    #[allow(clippy::needless_pass_by_value)] // generated WIT binding requires &Vec
    pub fn resume(
        &self,
        id: u64,
        results: Vec<wit_types::CalloutResult>,
    ) -> std::result::Result<wit_types::ProviderStep, Error> {
        without_tokio_handle(|| {
            let mut store = self.store.lock();
            Ok(self.bindings.omnifs_provider_continuation().call_resume(
                &mut *store,
                id,
                &results,
            )?)
        })
    }

    pub fn initialize(&self) -> std::result::Result<wit_types::ProviderReturn, Error> {
        without_tokio_handle(|| {
            let mut store = self.store.lock();
            Ok(self
                .bindings
                .omnifs_provider_lifecycle()
                .call_initialize(&mut *store, &self.config_bytes)?)
        })
    }

    pub fn shutdown(&self) -> std::result::Result<(), Error> {
        without_tokio_handle(|| {
            let mut store = self.store.lock();
            self.bindings
                .omnifs_provider_lifecycle()
                .call_shutdown(&mut *store)?;
            Ok(())
        })
    }

    /// The provider's self-describing manifest as a JSON string. Needs no config
    /// and runs no `start`, so the build-time manifest-embed tool can call it on
    /// any provider (including config/auth-gated ones) to harvest the full
    /// manifest before injecting the metadata custom section.
    pub fn manifest_json(&self) -> std::result::Result<String, Error> {
        without_tokio_handle(|| {
            let mut store = self.store.lock();
            Ok(self
                .bindings
                .omnifs_provider_lifecycle()
                .call_manifest_json(&mut *store)?)
        })
    }

    pub fn close_file(&self, handle: u64) -> std::result::Result<(), Error> {
        without_tokio_handle(|| {
            let mut store = self.store.lock();
            self.bindings
                .omnifs_provider_namespace()
                .call_close_file(&mut *store, handle)?;
            Ok(())
        })
    }
}

/// Run a synchronous wasmtime call without an ambient tokio
/// runtime handle.
///
/// The host uses `wasmtime_wasi::p2::add_to_linker_sync`. When the
/// guest reaches into WASI (preopened files, sockets, etc.) the shim
/// calls `tokio::runtime::Handle::try_current()`. If a handle is
/// present, the shim does `handle.block_on(future)`, which panics on
/// a tokio worker thread with "Cannot start a runtime from within a
/// runtime".
///
/// FUSE callbacks reach this code through `rt.block_on(...)`, so the
/// tokio handle is current here. We hop to a fresh OS thread that
/// has no handle; `try_current()` returns `Err` there and the WASI
/// shim falls through to its standalone `RUNTIME` singleton.
///
/// `Store<T>` is `Send` as long as the `WasiView` data is, and the
/// lock guard never crosses the thread boundary because the closure
/// owns it. This is a per-call thread spawn; the cost is one OS
/// thread per FUSE op, which is acceptable for the current latency
/// budget. A long-lived owner thread per provider is the obvious
/// next optimisation.
fn without_tokio_handle<R, F>(f: F) -> R
where
    F: FnOnce() -> R + Send,
    R: Send,
{
    if tokio::runtime::Handle::try_current().is_err() {
        return f();
    }
    std::thread::scope(|s| s.spawn(f).join().expect("wasmtime worker thread panicked"))
}

fn build_wasi_ctx(
    preopens: &[PreopenedPath],
) -> std::result::Result<wasmtime_wasi::WasiCtx, BuildError> {
    let mut builder = WasiCtxBuilder::new();
    for entry in preopens {
        let host = validate_preopen_path(&entry.host, "host")?;
        // Guest paths must also be absolute. They share the same
        // no-parent-escape rule because Wasmtime's preopen API treats
        // them as opaque mount tokens; relative or `..`-laden values
        // would silently confuse later guest-side path resolution.
        let _ = validate_preopen_path(&entry.guest, "guest")?;
        let (dir_perms, file_perms) = match entry.mode {
            PreopenMode::Ro => (DirPerms::READ, FilePerms::READ),
            PreopenMode::Rw => (
                DirPerms::READ | DirPerms::MUTATE,
                FilePerms::READ | FilePerms::WRITE,
            ),
        };
        builder
            .preopened_dir(&host, &entry.guest, dir_perms, file_perms)
            .map_err(|e| {
                BuildError::InvalidConfig(format!(
                    "preopen failed for host={} guest={}: {e}",
                    host.display(),
                    entry.guest,
                ))
            })?;
    }
    Ok(builder.build())
}

fn validate_preopen_path(raw: &str, label: &str) -> std::result::Result<PathBuf, BuildError> {
    let path = PathBuf::from(raw);
    if !path.is_absolute() {
        return Err(BuildError::InvalidConfig(format!(
            "preopen {label} path must be absolute: {raw}"
        )));
    }
    if path
        .components()
        .any(|c| matches!(c, PathComponent::ParentDir))
    {
        return Err(BuildError::InvalidConfig(format!(
            "preopen {label} path must not contain '..' segments: {raw}"
        )));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validate_preopen_path_rejects_relative() {
        let err = validate_preopen_path("data/db", "host").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("must be absolute"), "unexpected error: {msg}");
    }

    #[test]
    fn validate_preopen_path_rejects_parent_dir() {
        let err = validate_preopen_path("/data/../etc/passwd", "host").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("must not contain '..'"),
            "unexpected error: {msg}"
        );
    }
}
