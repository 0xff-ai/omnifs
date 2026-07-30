//! Process-scoped runtime host: opened caches, engine, and cloner.

use std::path::PathBuf;
use std::sync::Arc;

use crate::cache::Caches;
use crate::cloner::GitCloner;
use crate::runtime::wasm::ComponentEngine;

/// Daemon-state inputs that do not rely on workspace credential or provider
/// stores. Durable generation inputs carry both values explicitly.
pub struct HostRuntimeOpen {
    pub projection: PathBuf,
    pub clones: PathBuf,
    pub engine: ComponentEngine,
}

#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("provider engine init: {0}")]
    Engine(#[from] wasmtime::Error),
    #[error("cache open: {0}")]
    Cache(#[source] anyhow::Error),
    #[error("git clone cache: {0}")]
    Cloner(#[source] std::io::Error),
}

/// Online host: provider engine, projection caches, and a fetch-capable cloner.
pub struct HostOnline {
    caches: Arc<Caches>,
    engine: ComponentEngine,
    cloner: Arc<GitCloner>,
}

impl HostOnline {
    pub fn open_runtime(open: HostRuntimeOpen) -> Result<Self, HostError> {
        let HostRuntimeOpen {
            projection,
            clones,
            engine,
        } = open;
        let caches = Caches::open(&projection).map_err(HostError::Cache)?;
        let cloner = Arc::new(GitCloner::new(&clones).map_err(HostError::Cloner)?);
        Ok(Self {
            caches,
            engine,
            cloner,
        })
    }

    #[must_use]
    pub fn caches(&self) -> &Arc<Caches> {
        &self.caches
    }

    #[must_use]
    pub fn engine(&self) -> &ComponentEngine {
        &self.engine
    }

    #[must_use]
    pub fn cloner(&self) -> &Arc<GitCloner> {
        &self.cloner
    }
}
