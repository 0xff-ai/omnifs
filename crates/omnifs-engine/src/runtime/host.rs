//! Process-scoped runtime host: opened caches, engine, credentials, and cloner.
//!
//! [`omnifs_workspace::Workspace`] owns durable path-backed stores. [`Host`]
//! opens the daemon-only runtime over those handles. Online and offline are
//! distinct variants so offline cannot touch Wasmtime or credentials.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use omnifs_auth::{AuthError, CredentialService, OAuthClient};
use omnifs_workspace::creds::{CredentialStore, FileStore};
use omnifs_workspace::provider::Catalog;

use crate::cache::Caches;
use crate::cloner::{CloneError, GitCloner};
use crate::runtime::wasm::ComponentEngine;

/// Inputs to open an online [`HostOnline`].
pub struct HostOpen {
    pub cache_dir: PathBuf,
    pub wasm_cache_dir: PathBuf,
    pub credentials: Arc<FileStore>,
    pub catalog: Catalog,
    pub clone_dir: PathBuf,
}

/// Inputs to open an offline [`HostOffline`]: durable cache only.
pub struct HostOfflineOpen {
    pub cache_dir: PathBuf,
    pub clone_dir: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("provider engine init: {0}")]
    Engine(#[from] wasmtime::Error),
    #[error("cache open: {0}")]
    Cache(#[source] anyhow::Error),
    #[error("offline cache open failed: {0}")]
    OfflineCache(String),
    #[error("credential service init: {0}")]
    Credentials(#[from] AuthError),
    #[error("git clone cache: {0}")]
    Cloner(#[source] std::io::Error),
    #[error("offline git clone cache: {0}")]
    OfflineCloner(#[from] CloneError),
}

/// Online host: providers, credentials, and a fetch-capable cloner.
pub struct HostOnline {
    caches: Arc<Caches>,
    engine: ComponentEngine,
    credentials: Arc<CredentialService>,
    cloner: Arc<GitCloner>,
    catalog: Catalog,
    cache_dir: PathBuf,
    clone_dir: PathBuf,
}

/// Offline host: existing caches and optional existing clones only.
pub struct HostOffline {
    caches: Arc<Caches>,
    cache_dir: PathBuf,
    clone_dir: PathBuf,
    cloner: OnceLock<Arc<GitCloner>>,
}

/// Process-scoped runtime opened from a workspace (or test dirs).
pub enum Host {
    Online(HostOnline),
    Offline(HostOffline),
}

impl Host {
    #[must_use]
    pub fn is_offline(&self) -> bool {
        matches!(self, Self::Offline(_))
    }

    #[must_use]
    pub fn cache_dir(&self) -> &Path {
        match self {
            Self::Online(host) => host.cache_dir(),
            Self::Offline(host) => host.cache_dir(),
        }
    }
}

impl HostOnline {
    pub fn open(open: HostOpen) -> Result<Self, HostError> {
        let engine = ComponentEngine::new(Some(&open.wasm_cache_dir))?;
        let caches = Caches::open(&open.cache_dir).map_err(HostError::Cache)?;
        let store: Arc<dyn CredentialStore> = open.credentials;
        let credentials = Arc::new(CredentialService::new(store, OAuthClient::new()?));
        let cloner = Arc::new(GitCloner::new(&open.clone_dir).map_err(HostError::Cloner)?);
        Ok(Self {
            caches,
            engine,
            credentials,
            cloner,
            catalog: open.catalog,
            cache_dir: open.cache_dir,
            clone_dir: open.clone_dir,
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
    pub fn credentials(&self) -> &Arc<CredentialService> {
        &self.credentials
    }

    #[must_use]
    pub fn cloner(&self) -> &Arc<GitCloner> {
        &self.cloner
    }

    #[must_use]
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    #[must_use]
    pub fn clone_dir(&self) -> &Path {
        &self.clone_dir
    }

    #[must_use]
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }
}

impl HostOffline {
    /// Fail-closed over durable facts already on disk. Does not construct
    /// Wasmtime, credentials, HTTP, or a fetch-capable cloner.
    pub fn open(open: HostOfflineOpen) -> Result<Self, HostError> {
        let caches = Caches::open_existing(&open.cache_dir)
            .map_err(|error| HostError::OfflineCache(error.to_string()))?;
        Ok(Self {
            caches,
            cache_dir: open.cache_dir,
            clone_dir: open.clone_dir,
            cloner: OnceLock::new(),
        })
    }

    /// Reuse an already-open cache for offline validation (no second open).
    pub(crate) fn with_open_caches(caches: Arc<Caches>, clone_dir: PathBuf) -> Self {
        Self {
            caches,
            cache_dir: PathBuf::new(),
            clone_dir,
            cloner: OnceLock::new(),
        }
    }

    #[must_use]
    pub fn caches(&self) -> &Arc<Caches> {
        &self.caches
    }

    #[must_use]
    pub fn clone_dir(&self) -> &Path {
        &self.clone_dir
    }

    #[must_use]
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    pub fn ensure_cloner(&self) -> Result<&Arc<GitCloner>, HostError> {
        if self.cloner.get().is_none() {
            let opened = Arc::new(GitCloner::open_existing(&self.clone_dir)?);
            let _ = self.cloner.set(opened);
        }
        Ok(self.cloner.get().expect("offline cloner initialized"))
    }
}
