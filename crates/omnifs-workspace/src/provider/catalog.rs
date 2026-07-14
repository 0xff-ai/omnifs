//! The provider catalog: a read-only index over the content-addressed
//! [`ProviderStore`].
//!
//! `Catalog` answers "which retained artifact is this exact pinned id?" Each
//! [`Provider`] it yields lazily reads its embedded manifest from the retained
//! WASM. Nothing here touches mount specs; resolution (joining a spec against
//! this index) lives in `omnifs-mount`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::ids::{ProviderId, ProviderMeta, ProviderRef};

use crate::provider::store::{IndexEntry, ProviderStore, StoreError};
use crate::provider::{ProviderManifest, ProviderMetadataError, read_provider_metadata_section};

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("provider store error: {0}")]
    Store(#[from] StoreError),
    #[error("failed to extract provider metadata from {}: {source}", path.display())]
    ExtractProviderMetadata {
        path: PathBuf,
        source: ProviderMetadataError,
    },
    #[error("provider artifact at {} has no embedded metadata section", path.display())]
    MissingProviderMetadata { path: PathBuf },
    #[error("provider artifact at {} is not a regular file", path.display())]
    NonRegularArtifact { path: PathBuf },
    #[error("provider artifact at {} does not match its indexed digest {expected}", path.display())]
    Integrity {
        path: PathBuf,
        expected: ProviderId,
        actual: ProviderId,
    },
    #[error(
        "provider artifact at {} has manifest {field} `{actual}`, indexed as `{expected}`",
        path.display()
    )]
    ManifestMismatch {
        path: PathBuf,
        field: &'static str,
        expected: String,
        actual: String,
    },
    #[error("failed to read provider artifact from {}: {source}", path.display())]
    ReadArtifact {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// A read-only view over the providers retained under one store root.
#[derive(Debug, Clone)]
pub struct Catalog {
    store: ProviderStore,
}

impl Catalog {
    #[must_use]
    pub fn open(providers_dir: impl AsRef<Path>) -> Self {
        Self {
            store: ProviderStore::new(providers_dir.as_ref()),
        }
    }

    fn provider_from_entry(&self, entry: &IndexEntry) -> Provider {
        Provider {
            id: entry.id,
            meta: ProviderMeta {
                name: entry.name.clone(),
                version: entry.version.clone(),
            },
            wasm_path: self.store.artifact_path(&entry.id),
        }
    }

    /// Resolve an exact pinned id to its retained artifact. `None` means the
    /// index entry or regular artifact file is absent; a present but incoherent
    /// claim is an error.
    pub fn get(&self, id: &ProviderId) -> Result<Option<Provider>, CatalogError> {
        let index = self.store.read_index()?;
        let Some(entry) = index.providers.iter().find(|entry| &entry.id == id) else {
            return Ok(None);
        };
        let path = self.store.artifact_path(id);
        let Some(bytes) = read_artifact_file(&path)? else {
            return Ok(None);
        };
        let actual = ProviderId::from_wasm_bytes(&bytes);
        if actual != *id {
            return Err(CatalogError::Integrity {
                path,
                expected: *id,
                actual,
            });
        }
        let manifest = read_provider_metadata_bytes(&path, &bytes)?
            .ok_or_else(|| CatalogError::MissingProviderMetadata { path: path.clone() })?;
        let actual_name = manifest.id.clone();
        if actual_name != entry.name.as_str() {
            return Err(CatalogError::ManifestMismatch {
                path,
                field: "name",
                expected: entry.name.to_string(),
                actual: actual_name,
            });
        }
        let expected_version = entry.version.as_ref().map(|version| version.as_str());
        let actual_version = manifest.version.as_deref();
        if actual_version != expected_version {
            return Err(CatalogError::ManifestMismatch {
                path,
                field: "version",
                expected: expected_version.unwrap_or("<none>").to_owned(),
                actual: actual_version.unwrap_or("<none>").to_owned(),
            });
        }
        let provider = self.provider_from_entry(entry);
        Ok(Some(provider))
    }

    /// `<hex>.wasm` for a pinned id (the serving path).
    #[must_use]
    pub fn provider_path_by_id(&self, id: &ProviderId) -> PathBuf {
        self.store.artifact_path(id)
    }

    /// The state of the backing providers directory: absent, present with a
    /// retained-artifact count, or present but with an unreadable index.
    #[must_use]
    pub fn dir_status(&self) -> DirStatus {
        if !self.store.root().exists() {
            return DirStatus::Missing;
        }
        match self.store.read_index() {
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
/// meta, and a lazily-read handle to the retained WASM.
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

    /// `<hex>.wasm` path of this artifact.
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
}

fn read_provider_metadata_file(path: &Path) -> Result<Option<ProviderManifest>, CatalogError> {
    let Some(bytes) = read_artifact_file(path)? else {
        return Ok(None);
    };
    read_provider_metadata_bytes(path, &bytes)
}

fn read_artifact_file(path: &Path) -> Result<Option<Vec<u8>>, CatalogError> {
    let file_type = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata.file_type(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(CatalogError::ReadArtifact {
                path: path.to_path_buf(),
                source,
            });
        },
    };
    if !file_type.is_file() {
        return Err(CatalogError::NonRegularArtifact {
            path: path.to_path_buf(),
        });
    }
    fs::read(path)
        .map(Some)
        .map_err(|source| CatalogError::ReadArtifact {
            path: path.to_path_buf(),
            source,
        })
}

fn read_provider_metadata_bytes(
    path: &Path,
    bytes: &[u8],
) -> Result<Option<ProviderManifest>, CatalogError> {
    read_provider_metadata_section(bytes).map_err(|source| CatalogError::ExtractProviderMetadata {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::ProviderName;
    use crate::provider::Artifact;
    use crate::provider::sections::wasm_with_provider_metadata;
    use tempfile::tempdir;

    const EMPTY_WASM: &[u8] = b"\0asm\x01\0\0\0";

    fn fixture_artifact(name: &str) -> Artifact {
        let metadata = serde_json::json!({
            "id": name,
            "displayName": name,
            "provider": format!("{name}.wasm"),
            "defaultMount": name
        });
        let bytes = wasm_with_provider_metadata(
            EMPTY_WASM,
            serde_json::to_vec(&metadata).unwrap().as_slice(),
        );
        Artifact::from_bytes(format!("{name}.wasm"), bytes).unwrap()
    }

    #[test]
    fn get_requires_exact_retained_bytes_and_matching_manifest() {
        let dir = tempdir().unwrap();
        let store = ProviderStore::new(dir.path());
        let artifact = fixture_artifact("demo");
        let id = artifact.id();
        store.retain(&artifact).unwrap();
        let catalog = Catalog::open(dir.path());

        assert_eq!(catalog.get(&id).unwrap().unwrap().id, id);

        let path = store.artifact_path(&id);
        std::fs::write(&path, b"corrupt").unwrap();
        assert!(matches!(
            catalog.get(&id),
            Err(CatalogError::Integrity { expected, .. }) if expected == id
        ));

        std::fs::write(&path, &artifact.bytes).unwrap();

        let mut index = store.read_index().unwrap();
        index.providers[0].name = ProviderName::new("other").unwrap();
        let index_bytes = serde_json::to_vec_pretty(&index).unwrap();
        std::fs::write(dir.path().join("index.json"), index_bytes).unwrap();
        assert!(matches!(
            catalog.get(&id),
            Err(CatalogError::ManifestMismatch { field: "name", .. })
        ));
    }
}
