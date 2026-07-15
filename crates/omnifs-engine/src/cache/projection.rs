//! Durable projection identity and strict projection manifests.

use super::identity::ProjectionId;
use fjall::Readable;
use fjall::{
    KeyspaceCreateOptions, OptimisticTxDatabase, OptimisticTxKeyspace, OptimisticWriteTx,
    PersistMode,
};
use omnifs_workspace::ids::ProviderId;
use omnifs_workspace::mounts::Name;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static MANIFEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub const PROJECTION_MANIFEST_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProjectionManifest {
    pub version: u32,
    pub mount: Name,
    pub spec_digest: String,
    pub provider_id: ProviderId,
}

impl ProjectionManifest {
    fn new(mount: &Name, spec_source: &[u8], provider_id: ProviderId) -> Self {
        Self {
            version: PROJECTION_MANIFEST_VERSION,
            mount: mount.clone(),
            spec_digest: blake3::hash(spec_source).to_hex().to_string(),
            provider_id,
        }
    }

    fn validate(
        &self,
        mount: &Name,
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
    root: PathBuf,
    manifest: ProjectionManifest,
    db: OptimisticTxDatabase,
    facts: OptimisticTxKeyspace,
}

pub(crate) struct ProjectionRow {
    pub key: Vec<u8>,
    pub value: Vec<u8>,
}

impl ProjectionStore {
    pub(crate) fn open(
        root: impl AsRef<Path>,
        database: &OptimisticTxDatabase,
        id: ProjectionId,
        mount: &Name,
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
            if name.starts_with(".manifest-") && name.ends_with(".tmp") {
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
                    use std::io::Write as _;
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
            root,
            manifest,
            db: database.clone(),
            facts,
        })
    }

    /// Open one exact projection without creating its directory, manifest, or
    /// facts keyspace and without sweeping publication temporaries.
    pub(crate) fn open_existing(
        root: impl AsRef<Path>,
        database: &OptimisticTxDatabase,
        id: ProjectionId,
        mount: &Name,
        spec_source: &[u8],
        provider_id: ProviderId,
    ) -> Result<Self, ProjectionStoreError> {
        if id != ProjectionId::new(spec_source, provider_id) {
            return Err(ProjectionStoreError::InvalidIdentity);
        }
        let root =
            crate::cache::existing_directory(&root.as_ref().join(id.hex())).map_err(|error| {
                if error.kind() == io::ErrorKind::NotFound {
                    ProjectionStoreError::Missing
                } else {
                    ProjectionStoreError::Io(error)
                }
            })?;
        let manifest_path = root.join("manifest.json");
        let metadata = fs::symlink_metadata(&manifest_path).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                ProjectionStoreError::Missing
            } else {
                ProjectionStoreError::Io(error)
            }
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(ProjectionStoreError::InvalidManifest);
        }
        let bytes = read_manifest(&manifest_path)?;
        let manifest: ProjectionManifest =
            serde_json::from_slice(&bytes).map_err(ProjectionStoreError::Manifest)?;
        if manifest.version != PROJECTION_MANIFEST_VERSION {
            return Err(ProjectionStoreError::Version(manifest.version));
        }
        manifest.validate(mount, spec_source, provider_id)?;
        let keyspace = format!("facts.{}", id.hex());
        if !database.keyspace_exists(&keyspace) {
            return Err(ProjectionStoreError::Missing);
        }
        let facts = database.keyspace(&keyspace, KeyspaceCreateOptions::default)?;
        Ok(Self {
            root,
            manifest,
            db: database.clone(),
            facts,
        })
    }

    #[must_use]
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub(crate) fn manifest(&self) -> &ProjectionManifest {
        &self.manifest
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

    pub(crate) fn rows(&self) -> Result<Vec<ProjectionRow>, ProjectionStoreError> {
        let snapshot = self.db.read_tx();
        snapshot
            .iter(&self.facts)
            .map(|guard| {
                let (key, value) = guard.into_inner()?;
                Ok(ProjectionRow {
                    key: key.to_vec(),
                    value: value.to_vec(),
                })
            })
            .collect()
    }

    pub(crate) fn transact<F, T>(&self, mut plan: F) -> Result<T, ProjectionStoreError>
    where
        F: FnMut(&mut OptimisticWriteTx, &OptimisticTxKeyspace) -> Result<T, ProjectionStoreError>,
    {
        for _ in 0..8 {
            let mut tx = self.db.write_tx()?.durability(Some(PersistMode::SyncAll));
            let result = plan(&mut tx, &self.facts)?;
            match tx.commit()? {
                Ok(()) => return Ok(result),
                Err(_) => continue,
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
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::{
        PROJECTION_MANIFEST_VERSION, ProjectionManifest, ProjectionStore, ProjectionStoreError,
    };
    use crate::cache::identity::ProjectionId;
    use omnifs_workspace::ids::ProviderId;
    use omnifs_workspace::mounts::Name;

    #[test]
    fn manifest_rejects_wrong_identity_and_version() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("projections");
        let database = fjall::OptimisticTxDatabase::builder(temp.path().join("database"))
            .open()
            .unwrap();
        let mount = Name::new("test").unwrap();
        let source = br#"{"mount":"test"}"#;
        let provider = ProviderId::from_wasm_bytes(b"provider");
        let id = ProjectionId::new(source, provider);
        let wrong = ProjectionId::new(b"other", provider);
        assert!(matches!(
            ProjectionStore::open(&root, &database, wrong, &mount, source, provider),
            Err(ProjectionStoreError::InvalidIdentity)
        ));

        let valid = ProjectionStore::open(&root, &database, id, &mount, source, provider)
            .expect("create current projection");
        drop(valid);
        ProjectionStore::open_existing(&root, &database, id, &mount, source, provider)
            .expect("open current projection without creating it");
        assert!(matches!(
            ProjectionStore::open_existing(&root, &database, wrong, &mount, source, provider),
            Err(ProjectionStoreError::InvalidIdentity)
        ));

        let projection_root = crate::cache::canonical_directory(&root.join(id.hex())).unwrap();
        let mut manifest = ProjectionManifest::new(&mount, source, provider);
        manifest.mount = Name::new("other").unwrap();
        std::fs::write(
            projection_root.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            ProjectionStore::open_existing(&root, &database, id, &mount, source, provider),
            Err(ProjectionStoreError::ManifestMismatch)
        ));

        manifest = ProjectionManifest::new(&mount, source, provider);
        manifest.version = PROJECTION_MANIFEST_VERSION + 1;
        std::fs::write(
            projection_root.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            ProjectionStore::open_existing(&root, &database, id, &mount, source, provider),
            Err(ProjectionStoreError::Version(version)) if version == PROJECTION_MANIFEST_VERSION + 1
        ));
    }
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
    #[error("the selected durable projection does not exist")]
    Missing,
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
