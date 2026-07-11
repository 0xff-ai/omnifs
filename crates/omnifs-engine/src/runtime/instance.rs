//! `Instance` — the Wasmtime mechanics boundary.
//!
//! Owns the wasm store, the generated bindings, and the serialized
//! provider config. A dedicated driver thread keeps Wasmtime's concurrent
//! store event loop alive so independent host tasks can start provider
//! calls while earlier calls are suspended on async host imports.
//! `Runtime` composes this with orchestration concerns
//! (executors, caches, activity, invalidation, coalesce).

use std::path::{Component as PathComponent, Path, PathBuf};
use std::sync::Arc;

use crate::callouts::{CalloutHost, ParkSignal};
use futures::StreamExt;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtxBuilder};

use crate::Provider;
use crate::wasi::HostState;
use crate::{BuildError, EngineError, Op};
use omnifs_caps::{PreopenMode, PreopenedPath};
use omnifs_wit::provider::types as wit_types;

#[derive(Clone)]
pub struct Instance {
    tx: tokio::sync::mpsc::UnboundedSender<Command>,
    config_bytes: Vec<u8>,
}

enum Command {
    SetCallouts {
        callouts: CalloutHost,
        reply: std::sync::mpsc::Sender<std::result::Result<(), EngineError>>,
    },
    Initialize {
        config_bytes: Vec<u8>,
        reply: std::sync::mpsc::Sender<std::result::Result<wit_types::ProviderReturn, EngineError>>,
    },
    StartOp {
        op: Op,
        id: u64,
        reply: tokio::sync::oneshot::Sender<
            std::result::Result<wit_types::ProviderReturn, EngineError>,
        >,
    },
    Shutdown {
        reply: std::sync::mpsc::Sender<std::result::Result<(), EngineError>>,
    },
    CloseFile {
        handle: u64,
        reply: std::sync::mpsc::Sender<std::result::Result<(), EngineError>>,
    },
}

impl Instance {
    pub(crate) fn new(
        engine: &wasmtime::Engine,
        wasm_path: &Path,
        config_bytes: Vec<u8>,
        preopens: &[PreopenedPath],
        park_signal: Option<ParkSignal>,
    ) -> std::result::Result<Self, BuildError> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let engine = engine.clone();
        let wasm_path = wasm_path.to_path_buf();
        let preopens = preopens.to_vec();

        std::thread::Builder::new()
            .name("omnifs-provider-instance".to_string())
            .spawn(move || {
                let mut builder = tokio::runtime::Builder::new_current_thread();
                builder.enable_all();
                // Test capture only: signal the harness each time this
                // single-threaded executor goes idle, so it can close a
                // captured callout burst on the executor's real quiescence
                // boundary rather than a timing heuristic. `None` in
                // production, where nothing observes callout bursts.
                if let Some(park_signal) = park_signal {
                    builder.on_thread_park(move || park_signal.notify());
                }
                let runtime = match builder.build() {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = ready_tx.send(Err(BuildError::ProviderProtocol(format!(
                            "provider driver runtime: {error}"
                        ))));
                        return;
                    },
                };
                runtime.block_on(async move {
                    match build_driver_state(&engine, &wasm_path, &preopens).await {
                        Ok((store, bindings)) => {
                            let _ = ready_tx.send(Ok(()));
                            if let Err(error) = drive_instance(store, bindings, rx).await {
                                tracing::error!(error = %error, "provider instance driver exited");
                            }
                        },
                        Err(error) => {
                            let _ = ready_tx.send(Err(error));
                        },
                    }
                });
            })
            .map_err(|error| {
                BuildError::ProviderProtocol(format!("spawn provider driver: {error}"))
            })?;

        ready_rx.recv().map_err(|error| {
            BuildError::ProviderProtocol(format!("provider driver did not start: {error}"))
        })??;

        Ok(Self { tx, config_bytes })
    }

    pub(crate) async fn start_op(
        &self,
        op: Op,
        id: u64,
    ) -> std::result::Result<wit_types::ProviderReturn, EngineError> {
        let (reply, recv) = tokio::sync::oneshot::channel();
        self.tx
            .send(Command::StartOp { op, id, reply })
            .map_err(|_| {
                EngineError::ProviderProtocol("provider instance driver stopped".to_string())
            })?;
        recv.await.map_err(|_| {
            EngineError::ProviderProtocol("provider operation reply dropped".to_string())
        })?
    }

    pub fn initialize(&self) -> std::result::Result<wit_types::ProviderReturn, EngineError> {
        self.call_sync(|reply| Command::Initialize {
            config_bytes: self.config_bytes.clone(),
            reply,
        })
    }

    pub(crate) fn set_callouts(
        &self,
        callouts: CalloutHost,
    ) -> std::result::Result<(), EngineError> {
        self.call_sync(|reply| Command::SetCallouts { callouts, reply })
    }

    pub fn shutdown(&self) -> std::result::Result<(), EngineError> {
        self.call_sync(|reply| Command::Shutdown { reply })
    }

    pub fn close_file(&self, handle: u64) -> std::result::Result<(), EngineError> {
        self.call_sync(|reply| Command::CloseFile { handle, reply })
    }

    fn call_sync<T>(
        &self,
        build: impl FnOnce(std::sync::mpsc::Sender<std::result::Result<T, EngineError>>) -> Command,
    ) -> std::result::Result<T, EngineError> {
        let (reply, recv) = std::sync::mpsc::channel();
        self.tx.send(build(reply)).map_err(|_| {
            EngineError::ProviderProtocol("provider instance driver stopped".to_string())
        })?;
        recv.recv().map_err(|_| {
            EngineError::ProviderProtocol("provider instance reply dropped".to_string())
        })?
    }
}

async fn build_driver_state(
    engine: &wasmtime::Engine,
    wasm_path: &Path,
    preopens: &[PreopenedPath],
) -> std::result::Result<(wasmtime::Store<HostState>, Provider), BuildError> {
    let mut linker = Linker::<HostState>::new(engine);
    wasmtime_wasi::p2::add_to_linker_async::<HostState>(&mut linker)?;
    Provider::add_to_linker::<HostState, HostState>(&mut linker, |state| state)?;

    let component = Component::from_file(engine, wasm_path)?;
    let wasi = build_wasi_ctx(preopens)?;
    let mut store = wasmtime::Store::new(
        engine,
        HostState {
            wasi,
            table: ResourceTable::new(),
            callouts: None,
        },
    );

    let bindings = Provider::instantiate_async(&mut store, &component, &linker).await?;
    Ok((store, bindings))
}

async fn drive_instance(
    mut store: wasmtime::Store<HostState>,
    bindings: Provider,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<Command>,
) -> wasmtime::Result<()> {
    let bindings = Arc::new(bindings);
    store
        .run_concurrent(async |accessor| -> wasmtime::Result<()> {
            let mut calls = futures::stream::FuturesUnordered::new();
            loop {
                tokio::select! {
                    Some(command) = rx.recv() => {
                        match command {
                            Command::SetCallouts { callouts, reply } => {
                                accessor.with(|mut access| {
                                    access.get().callouts = Some(callouts);
                                });
                                let _ = reply.send(Ok(()));
                            },
                            Command::Initialize { config_bytes, reply } => {
                                let lifecycle = bindings.omnifs_provider_lifecycle();
                                let result = match accessor.with(|access| {
                                    lifecycle
                                        .func_initialize()
                                        .func()
                                        .typed::<(Vec<u8>,), (wit_types::ProviderReturn,)>(&access)
                                }) {
                                    Ok(initialize) => initialize
                                        .call_concurrent(accessor, (config_bytes,))
                                        .await
                                        .map(|(ret,)| ret),
                                    Err(error) => Err(error),
                                }
                                .map_err(Into::into);
                                let _ = reply.send(result);
                            },
                            Command::StartOp { op, id, reply } => {
                                let bindings = Arc::clone(&bindings);
                                calls.push(Box::pin(async move {
                                    let result = call_op(&bindings, accessor, op, id).await;
                                    let _ = reply.send(result);
                                }));
                            },
                            Command::Shutdown { reply } => {
                                let shutdown = bindings.omnifs_provider_lifecycle().func_shutdown();
                                let result = shutdown
                                    .call_concurrent(accessor, ())
                                    .await
                                    .map_err(Into::into);
                                let _ = reply.send(result);
                                break;
                            },
                            Command::CloseFile { handle, reply } => {
                                let close_file =
                                    bindings.omnifs_provider_namespace().func_close_file();
                                let result = close_file
                                    .call_concurrent(accessor, (handle,))
                                    .await
                                    .map_err(Into::into);
                                let _ = reply.send(result);
                            },
                        }
                    },
                    Some(()) = calls.next(), if !calls.is_empty() => {},
                    else => break,
                }
            }
            Ok(())
        })
        .await?
}

async fn call_op(
    bindings: &Provider,
    accessor: &wasmtime::component::Accessor<HostState>,
    op: Op,
    id: u64,
) -> std::result::Result<wit_types::ProviderReturn, EngineError> {
    let namespace = bindings.omnifs_provider_namespace();
    match op {
        Op::LookupChild { parent_path, name } => Ok(namespace
            .call_lookup_child(
                accessor,
                id,
                parent_path.as_str().to_string(),
                name.as_str().to_string(),
            )
            .await?),
        Op::ListChildren {
            path,
            cached_validator,
            cursor,
        } => Ok(namespace
            .call_list_children(
                accessor,
                id,
                path.as_str().to_string(),
                cached_validator,
                cursor,
            )
            .await?),
        Op::ReadFile {
            path,
            content_type,
            cached_canonical,
        } => Ok(namespace
            .call_read_file(
                accessor,
                id,
                path.as_str().to_string(),
                content_type,
                cached_canonical,
            )
            .await?),
        Op::OpenFile { path } => Ok(namespace
            .call_open_file(accessor, id, path.as_str().to_string())
            .await?),
        Op::ReadChunk {
            handle,
            offset,
            length,
        } => Ok(namespace
            .call_read_chunk(accessor, id, handle, offset, length)
            .await?),
        Op::OnEvent { event } => Ok(bindings
            .omnifs_provider_notify()
            .call_on_event(accessor, id, event)
            .await?),
        Op::Initialize => {
            unreachable!("Op::Initialize never reaches start_op; see Instance::initialize")
        },
    }
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
