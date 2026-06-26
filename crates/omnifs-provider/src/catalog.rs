//! The provider catalog: a read-only index over the content-addressed
//! [`ProviderStore`].
//!
//! `Catalog` answers "which retained artifact is this pinned id / the latest for
//! this name / every installed provider?" Each [`Provider`] it yields lazily
//! reads its embedded manifest from the by-hash WASM. Nothing here touches mount
//! specs; resolution (joining a spec against this index) lives in `omnifs-mount`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use omnifs_core::{ProviderId, ProviderMeta, ProviderName, ProviderRef};

use crate::store::{IndexEntry, ProviderStore, StoreError};
use crate::{
    AuthManifest, ProviderManifest, ProviderMetadataError, read_provider_metadata_section,
};

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("provider store error: {0}")]
    Store(#[from] StoreError),
    #[error("failed to read provider metadata from {}: {source}", path.display())]
    ReadProviderMetadata {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to extract provider metadata from {}: {source}", path.display())]
    ExtractProviderMetadata {
        path: PathBuf,
        source: ProviderMetadataError,
    },
    #[error("provider artifact at {} has no embedded metadata section", path.display())]
    MissingProviderMetadata { path: PathBuf },
}

/// A read-only view over the providers installed under one store root.
#[derive(Debug, Clone)]
pub struct Catalog {
    providers_dir: PathBuf,
}

impl Catalog {
    #[must_use]
    pub fn open(providers_dir: impl AsRef<Path>) -> Self {
        Self {
            providers_dir: providers_dir.as_ref().to_path_buf(),
        }
    }

    #[must_use]
    pub fn providers_dir(&self) -> &Path {
        &self.providers_dir
    }

    /// The content-addressed store backing this catalog.
    #[must_use]
    pub fn store(&self) -> ProviderStore {
        ProviderStore::new(&self.providers_dir)
    }

    fn provider_from_entry(&self, entry: &IndexEntry) -> Provider {
        Provider {
            id: entry.id,
            meta: ProviderMeta {
                name: entry.name.clone(),
                version: entry.version.clone(),
            },
            wasm_path: self.store().by_hash_path(&entry.id),
        }
    }

    /// Resolve a pinned id to its retained artifact. `None` means the artifact is
    /// not retained (the use site raises `ArtifactMissing`).
    pub fn get(&self, id: &ProviderId) -> Result<Option<Provider>, CatalogError> {
        let index = self.store().read_index()?;
        let Some(entry) = index.providers.iter().find(|entry| &entry.id == id) else {
            return Ok(None);
        };
        let provider = self.provider_from_entry(entry);
        Ok(provider.wasm_path().exists().then_some(provider))
    }

    /// The most recently installed artifact for a name. Init and upgrade only,
    /// never serving.
    pub fn latest_by_name(&self, name: &ProviderName) -> Result<Option<Provider>, CatalogError> {
        let index = self.store().read_index()?;
        let Some(id) = index.latest.get(name.as_str()) else {
            return Ok(None);
        };
        Ok(index
            .providers
            .iter()
            .find(|entry| &entry.id == id)
            .map(|entry| self.provider_from_entry(entry)))
    }

    /// Every installed artifact, including superseded versions.
    pub fn list(&self) -> Result<Vec<Provider>, CatalogError> {
        let index = self.store().read_index()?;
        Ok(index
            .providers
            .iter()
            .map(|entry| self.provider_from_entry(entry))
            .collect())
    }

    /// The latest installed artifact per provider name, in name order: the
    /// authoring/selection view (one selectable provider per name; older
    /// retained versions are upgrade history).
    pub fn installable(&self) -> Result<Vec<Provider>, CatalogError> {
        let index = self.store().read_index()?;
        Ok(index
            .latest
            .values()
            .filter_map(|id| index.providers.iter().find(|entry| &entry.id == id))
            .map(|entry| self.provider_from_entry(entry))
            .collect())
    }

    /// `by-hash/<hex>.wasm` for a pinned id (the serving path).
    #[must_use]
    pub fn provider_path_by_id(&self, id: &ProviderId) -> PathBuf {
        self.store().by_hash_path(id)
    }

    /// The state of the backing providers directory: absent, present with a
    /// retained-artifact count, or present but with an unreadable index.
    #[must_use]
    pub fn dir_status(&self) -> DirStatus {
        if !self.providers_dir.exists() {
            return DirStatus::Missing;
        }
        match self.store().read_index() {
            Ok(index) => DirStatus::Present {
                wasm_count: index.providers.len(),
            },
            Err(error) => DirStatus::Unreadable(error),
        }
    }
}

/// The state of a provider directory, reported by [`Catalog::dir_status`].
#[derive(Debug)]
pub enum DirStatus {
    /// The directory does not exist.
    Missing,
    /// The directory exists and its index lists `wasm_count` retained artifacts.
    Present { wasm_count: usize },
    /// The directory exists but its index could not be read.
    Unreadable(StoreError),
}

/// A retained provider artifact resolved from the store: content id, catalog/UI
/// meta, and a lazily-read handle to the by-hash WASM.
#[derive(Debug, Clone)]
pub struct Provider {
    pub id: ProviderId,
    pub meta: ProviderMeta,
    wasm_path: PathBuf,
}

impl Provider {
    /// The pinned reference the CLI writes into a mount spec.
    #[must_use]
    pub fn reference(&self) -> ProviderRef {
        ProviderRef {
            id: self.id,
            meta: self.meta.clone(),
        }
    }

    /// `by-hash/<hex>.wasm` path of this artifact.
    #[must_use]
    pub fn wasm_path(&self) -> &Path {
        &self.wasm_path
    }

    /// The provider manifest embedded in the artifact's metadata section.
    pub fn manifest(&self) -> Result<ProviderManifest, CatalogError> {
        read_provider_metadata_file(&self.wasm_path)?.ok_or_else(|| {
            CatalogError::MissingProviderMetadata {
                path: self.wasm_path.clone(),
            }
        })
    }

    /// The injection-only auth manifest derived from this artifact's manifest.
    pub fn auth_manifest(&self) -> Result<Option<AuthManifest>, CatalogError> {
        Ok(self.manifest()?.wasm_auth_manifest())
    }
}

fn read_provider_metadata_file(path: &Path) -> Result<Option<ProviderManifest>, CatalogError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(CatalogError::ReadProviderMetadata {
                path: path.to_path_buf(),
                source,
            });
        },
    };
    read_provider_metadata_section(&bytes).map_err(|source| CatalogError::ExtractProviderMetadata {
        path: path.to_path_buf(),
        source,
    })
}
