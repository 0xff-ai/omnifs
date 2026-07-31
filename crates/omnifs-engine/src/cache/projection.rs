//! Durable projection identity and strict projection manifests.

use super::identity::ProjectionId;
use fjall::Readable;
use fjall::{
    KeyspaceCreateOptions, OptimisticTxDatabase, OptimisticTxKeyspace, OptimisticWriteTx,
    PersistMode,
};
use omnifs_core::ProviderId;
use omnifs_core::ResourceName;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

static MANIFEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub const PROJECTION_MANIFEST_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProjectionManifest {
    pub version: u32,
    pub mount: ResourceName,
    pub spec_digest: String,
    pub provider_id: ProviderId,
}

impl ProjectionManifest {
    fn new(mount: &ResourceName, spec_source: &[u8], provider_id: ProviderId) -> Self {
        Self {
            version: PROJECTION_MANIFEST_VERSION,
            mount: mount.clone(),
            spec_digest: blake3::hash(spec_source).to_hex().to_string(),
            provider_id,
        }
    }

    fn validate(
        &self,
        mount: &ResourceName,
        spec_source: &[u8],
        provider_id: ProviderId,
    ) -> Result<(), ProjectionStoreError> {
        let expected = Self::new(mount, spec_source, provider_id);
        if self != &expected {
            return Err(ProjectionStoreError::ManifestMismatch);
        }
        Ok(())
    }
}

pub(crate) struct ProjectionStore {
    db: OptimisticTxDatabase,
    facts: OptimisticTxKeyspace,
}

impl ProjectionStore {
    pub(crate) fn open(
        root: impl AsRef<Path>,
        database: &OptimisticTxDatabase,
        id: ProjectionId,
        mount: &ResourceName,
        spec_source: &[u8],
        provider_id: ProviderId,
    ) -> Result<Self, ProjectionStoreError> {
        if id != ProjectionId::new(spec_source, provider_id) {
            return Err(ProjectionStoreError::InvalidIdentity);
        }
        let root = crate::cache::canonical_directory(&root.as_ref().join(id.hex()))?;
        crate::cache::ensure_directory(&root)?;
        let root_metadata = fs::symlink_metadata(&root)?;
        if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
            return Err(ProjectionStoreError::InvalidRoot);
        }
        for entry in fs::read_dir(&root)? {
            let path = entry?.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if name.starts_with(".manifest-")
                && Path::new(name).extension().is_some_and(|ext| ext == "tmp")
            {
                fs::remove_file(path)?;
            }
        }
        let path = root.join("manifest.json");
        let manifest = match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(ProjectionStoreError::InvalidManifest);
            },
            Ok(_) => {
                let bytes = read_manifest(&path)?;
                serde_json::from_slice(&bytes).map_err(ProjectionStoreError::Manifest)?
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let manifest = ProjectionManifest::new(mount, spec_source, provider_id);
                let bytes = serde_json::to_vec_pretty(&manifest)
                    .map_err(ProjectionStoreError::Serialize)?;
                let temporary = root.join(format!(
                    ".manifest-{}-{}.tmp",
                    std::process::id(),
                    MANIFEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
                ));
                let result = (|| {
                    let mut file = std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&temporary)?;
                    file.write_all(&bytes)?;
                    file.sync_all()?;
                    match fs::hard_link(&temporary, &path) {
                        Ok(()) => {
                            fs::remove_file(&temporary)?;
                            std::fs::File::open(&root)?.sync_all()?;
                            Ok(manifest.clone())
                        },
                        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                            let metadata = fs::symlink_metadata(&path)?;
                            if metadata.file_type().is_symlink() || !metadata.is_file() {
                                return Err(ProjectionStoreError::InvalidManifest);
                            }
                            let winner = read_manifest(&path)?;
                            let manifest = serde_json::from_slice(&winner)
                                .map_err(ProjectionStoreError::Manifest);
                            let removed = fs::remove_file(&temporary);
                            let synced = std::fs::File::open(&root)?.sync_all();
                            Ok({
                                let manifest = manifest?;
                                removed?;
                                synced?;
                                manifest
                            })
                        },
                        Err(error) => Err(error.into()),
                    }
                })();
                if result.is_err() {
                    let _ = fs::remove_file(&temporary);
                }
                result?
            },
            Err(error) => return Err(error.into()),
        };
        if manifest.version != PROJECTION_MANIFEST_VERSION {
            return Err(ProjectionStoreError::Version(manifest.version));
        }
        manifest.validate(mount, spec_source, provider_id)?;
        let facts = database.keyspace(
            &format!("facts.{}", id.hex()),
            KeyspaceCreateOptions::default,
        )?;
        Ok(Self {
            db: database.clone(),
            facts,
        })
    }

    pub(crate) fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, ProjectionStoreError> {
        Ok(self.facts.get(key)?.map(|value| value.to_vec()))
    }

    pub(crate) fn read_prefix(&self, prefix: &[u8]) -> Result<Vec<Vec<u8>>, ProjectionStoreError> {
        let tx = self.db.write_tx()?;
        tx.prefix(&self.facts, prefix)
            .map(|guard| Ok(guard.key()?.to_vec()))
            .collect()
    }

    pub(crate) fn transact<F, T>(&self, mut plan: F) -> Result<T, ProjectionStoreError>
    where
        F: FnMut(&mut OptimisticWriteTx, &OptimisticTxKeyspace) -> Result<T, ProjectionStoreError>,
    {
        for _ in 0..8 {
            let mut tx = self.db.write_tx()?.durability(Some(PersistMode::SyncAll));
            let result = plan(&mut tx, &self.facts)?;
            if let Ok(()) = tx.commit()? {
                return Ok(result);
            }
        }
        Err(ProjectionStoreError::Conflict)
    }
}

fn read_manifest(path: &Path) -> Result<Vec<u8>, ProjectionStoreError> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(path)?;
    let metadata = file.metadata()?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ProjectionStoreError::InvalidManifest);
    }
    let capacity =
        usize::try_from(metadata.len()).map_err(|_| ProjectionStoreError::InvalidManifest)?;
    let mut bytes = Vec::with_capacity(capacity);
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ProjectionStoreError {
    #[error("projection store I/O failed")]
    Io(#[source] io::Error),
    #[error("projection manifest is corrupt")]
    Manifest(#[source] serde_json::Error),
    #[error("projection manifest could not be serialized")]
    Serialize(#[source] serde_json::Error),
    #[error("projection manifest version {0} is unsupported")]
    Version(u32),
    #[error("projection manifest does not match the selected mount identity")]
    ManifestMismatch,
    #[error("projection store root is not a regular directory")]
    InvalidRoot,
    #[error("projection manifest is not a regular file")]
    InvalidManifest,
    #[error("projection directory does not match its spec and provider identity")]
    InvalidIdentity,
    #[error("projection database operation failed")]
    Fjall(#[source] fjall::Error),
    #[error("projection transaction conflicted repeatedly")]
    Conflict,
    #[error("projection transaction planning failed: {0}")]
    Transaction(String),
}

impl From<io::Error> for ProjectionStoreError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<fjall::Error> for ProjectionStoreError {
    fn from(error: fjall::Error) -> Self {
        Self::Fjall(error)
    }
}
