//! Content-addressed provider store.
//!
//! Layout under the store root (by default `~/.omnifs/providers/`):
//!
//! ```text
//! <64hex>.wasm   immutable provider artifacts, write-if-absent
//! index.json     retained artifact index
//! ```
//!
//! Artifacts are keyed by [`ProviderId`] (BLAKE3 of the exact WASM bytes), so a
//! present `<id>.wasm` is always the correct content. Retention appends an entry
//! under an advisory lock (same pattern as the credentials `FileStore`) so two
//! concurrent writers do not lose an update in the read-modify-write.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::ids::{ProviderId, ProviderName, ProviderVersion};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::provider::Artifact;

const INDEX_FILE: &str = "index.json";
const LOCK_FILE: &str = ".index.lock";
const INDEX_VERSION: u32 = 2;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("provider store io error at {}: {source}", path.display())]
    Io { path: PathBuf, source: io::Error },
    #[error("provider index at {} is corrupt: {source}", path.display())]
    Index {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("provider index at {} is not a regular file", path.display())]
    IndexNotRegular { path: PathBuf },
    #[error("unsupported provider index version {0}")]
    Version(u32),
    #[error("provider index contains duplicate provider id {0}")]
    DuplicateId(ProviderId),
    #[error("provider artifact at {} is not a regular file", path.display())]
    ArtifactNotRegular { path: PathBuf },
    #[error(
        "provider artifact at {} does not match requested digest {expected}; found {actual}",
        path.display()
    )]
    ArtifactMismatch {
        path: PathBuf,
        expected: ProviderId,
        actual: ProviderId,
    },
    #[error("failed to read provider artifact at {}: {source}", path.display())]
    ArtifactRead { path: PathBuf, source: io::Error },
    #[error("provider index lock at {} is not a regular file", path.display())]
    LockNotRegular { path: PathBuf },
}

/// One retained artifact in the index.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexEntry {
    pub id: ProviderId,
    pub name: ProviderName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<ProviderVersion>,
}

/// The provider index: every retained artifact, with no lifecycle state.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Index {
    pub version: u32,
    pub providers: Vec<IndexEntry>,
}

impl Index {
    #[must_use]
    fn empty() -> Self {
        Self {
            version: INDEX_VERSION,
            providers: Vec::new(),
        }
    }

    fn validate(&self) -> Result<(), StoreError> {
        let mut ids = HashSet::with_capacity(self.providers.len());
        for entry in &self.providers {
            if !ids.insert(entry.id) {
                return Err(StoreError::DuplicateId(entry.id));
            }
        }
        Ok(())
    }
}

/// The content-addressed store rooted at `providers_dir`.
#[derive(Debug, Clone)]
pub struct ProviderStore {
    root: PathBuf,
}

impl ProviderStore {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `root/<hex>.wasm`.
    #[must_use]
    pub fn artifact_path(&self, id: &ProviderId) -> PathBuf {
        self.root.join(format!("{id}.wasm"))
    }

    fn index_path(&self) -> PathBuf {
        self.root.join(INDEX_FILE)
    }

    /// Retain one validated provider artifact without replacing any existing
    /// artifact or selecting another version.
    ///
    /// Returns `true` only when this call inserts the provider's index entry.
    /// The result is decided while holding the store lock, so concurrent
    /// callers cannot both report a newly retained provider.
    pub fn retain(&self, artifact: &Artifact) -> Result<bool, StoreError> {
        let entry = IndexEntry {
            id: artifact.id,
            name: artifact.meta.name.clone(),
            version: artifact.meta.version.clone(),
        };
        create_dir_all(&self.root)?;
        let lock = self.lock()?;
        let result = self.retain_locked(artifact, entry);
        let _ = FileExt::unlock(&lock);
        result
    }

    /// Read the index, or an empty (version-stamped) one if it does not exist yet.
    pub fn read_index(&self) -> Result<Index, StoreError> {
        let path = self.index_path();
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Index::empty()),
            Err(source) => return Err(StoreError::Io { path, source }),
        };
        if !metadata.file_type().is_file() {
            return Err(StoreError::IndexNotRegular { path });
        }
        let bytes = fs::read(&path).map_err(|source| StoreError::Io {
            path: path.clone(),
            source,
        })?;
        let index: Index =
            serde_json::from_slice(&bytes).map_err(|source| StoreError::Index { path, source })?;
        if index.version != INDEX_VERSION {
            return Err(StoreError::Version(index.version));
        }
        index.validate()?;
        Ok(index)
    }

    fn retain_locked(&self, artifact: &Artifact, entry: IndexEntry) -> Result<bool, StoreError> {
        self.publish_artifact(artifact)?;
        let mut index = self.read_index()?;
        if index.providers.iter().any(|current| current.id == entry.id) {
            return Ok(false);
        }
        index.providers.push(entry);
        self.write_index(&index)?;
        Ok(true)
    }

    fn publish_artifact(&self, artifact: &Artifact) -> Result<(), StoreError> {
        let final_path = self.artifact_path(&artifact.id);
        match fs::symlink_metadata(&final_path) {
            Ok(metadata) => Self::validate_existing_artifact(&final_path, &metadata, artifact),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let tmp = self.write_artifact_temp(&artifact.id, &artifact.bytes)?;
                match fs::hard_link(&tmp, &final_path) {
                    Ok(()) => remove_temp(&tmp, &final_path),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                        remove_temp(&tmp, &final_path)?;
                        match fs::symlink_metadata(&final_path) {
                            Ok(metadata) => {
                                Self::validate_existing_artifact(&final_path, &metadata, artifact)
                            },
                            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                                self.publish_artifact(artifact)
                            },
                            Err(source) => Err(StoreError::ArtifactRead {
                                path: final_path,
                                source,
                            }),
                        }
                    },
                    Err(source) => {
                        let _ = fs::remove_file(&tmp);
                        Err(StoreError::Io {
                            path: final_path,
                            source,
                        })
                    },
                }
            },
            Err(source) => Err(StoreError::ArtifactRead {
                path: final_path,
                source,
            }),
        }
    }

    fn validate_existing_artifact(
        path: &Path,
        metadata: &fs::Metadata,
        artifact: &Artifact,
    ) -> Result<(), StoreError> {
        if !metadata.file_type().is_file() {
            return Err(StoreError::ArtifactNotRegular {
                path: path.to_path_buf(),
            });
        }
        let bytes = fs::read(path).map_err(|source| StoreError::ArtifactRead {
            path: path.to_path_buf(),
            source,
        })?;
        let actual = ProviderId::from_wasm_bytes(&bytes);
        if actual != artifact.id || bytes != artifact.bytes {
            return Err(StoreError::ArtifactMismatch {
                path: path.to_path_buf(),
                expected: artifact.id,
                actual,
            });
        }
        Ok(())
    }

    fn write_artifact_temp(&self, id: &ProviderId, bytes: &[u8]) -> Result<PathBuf, StoreError> {
        self.write_temp(&format!(".{id}.wasm.tmp"), bytes)
    }

    fn write_temp(&self, prefix: &str, bytes: &[u8]) -> Result<PathBuf, StoreError> {
        loop {
            let path = self.root.join(format!(
                "{prefix}-{}-{}",
                std::process::id(),
                TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            let mut file = match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(source) => {
                    return Err(StoreError::Io { path, source });
                },
            };
            if let Err(source) = file.write_all(bytes) {
                let _ = fs::remove_file(&path);
                return Err(StoreError::Io { path, source });
            }
            if let Err(source) = file.sync_all() {
                let _ = fs::remove_file(&path);
                return Err(StoreError::Io { path, source });
            }
            return Ok(path);
        }
    }

    fn write_index(&self, index: &Index) -> Result<(), StoreError> {
        let path = self.index_path();
        let bytes = serde_json::to_vec_pretty(index).map_err(|source| StoreError::Index {
            path: path.clone(),
            source,
        })?;
        let tmp = self.write_temp(&format!("{INDEX_FILE}.tmp"), &bytes)?;
        fs::rename(&tmp, &path).map_err(|source| StoreError::Io { path, source })
    }

    fn lock(&self) -> Result<File, StoreError> {
        let path = self.root.join(LOCK_FILE);
        let lock = loop {
            match OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(lock) => break lock,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    let metadata = match fs::symlink_metadata(&path) {
                        Ok(metadata) => metadata,
                        Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                        Err(source) => {
                            return Err(StoreError::Io {
                                path: path.clone(),
                                source,
                            });
                        },
                    };
                    if !metadata.file_type().is_file() {
                        return Err(StoreError::LockNotRegular { path });
                    }
                    break OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(&path)
                        .map_err(|source| StoreError::Io {
                            path: path.clone(),
                            source,
                        })?;
                },
                Err(source) => {
                    return Err(StoreError::Io {
                        path: path.clone(),
                        source,
                    });
                },
            }
        };
        lock.lock_exclusive()
            .map_err(|source| StoreError::Io { path, source })?;
        Ok(lock)
    }
}

fn create_dir_all(path: &Path) -> Result<(), StoreError> {
    fs::create_dir_all(path).map_err(|source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn remove_temp(path: &Path, final_path: &Path) -> Result<(), StoreError> {
    fs::remove_file(path).map_err(|source| StoreError::Io {
        path: final_path.to_path_buf(),
        source,
    })
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Artifact;
    use crate::provider::sections::wasm_with_provider_metadata;
    use tempfile::tempdir;

    const EMPTY_WASM: &[u8] = b"\0asm\x01\0\0\0";

    fn artifact(name: &str) -> (ProviderId, Artifact) {
        let file = format!("{name}.wasm");
        let metadata = serde_json::json!({
            "id": name,
            "displayName": name,
            "provider": &file,
            "defaultMount": name,
            "refreshIntervalSecs": 0
        });
        let bytes = wasm_with_provider_metadata(
            EMPTY_WASM,
            serde_json::to_vec(&metadata).unwrap().as_slice(),
        );
        let id = ProviderId::from_wasm_bytes(&bytes);
        (id, Artifact::from_bytes(file, bytes).unwrap())
    }

    #[test]
    fn retain_writes_root_hash_file_and_indexes_metadata() {
        let dir = tempdir().unwrap();
        let store = ProviderStore::new(dir.path());
        let (id, artifact) = artifact("demo");

        assert!(store.retain(&artifact).unwrap());

        assert!(store.artifact_path(&id).is_file());
        assert!(!dir.path().join("by-hash").exists());
        let index = store.read_index().unwrap();
        assert_eq!(index.providers.len(), 1);
        assert_eq!(index.providers[0].id, id);
        assert_eq!(index.providers[0].name.as_str(), "demo");
    }

    #[test]
    fn retain_is_content_addressed_and_idempotent() {
        let dir = tempdir().unwrap();
        let store = ProviderStore::new(dir.path());
        let (id, artifact) = artifact("demo");

        assert!(store.retain(&artifact).unwrap());
        assert!(!store.retain(&artifact).unwrap());
        assert_eq!(
            std::fs::read(store.artifact_path(&id)).unwrap(),
            artifact.bytes
        );
        assert_eq!(store.read_index().unwrap().providers.len(), 1);
    }

    #[test]
    fn retain_records_same_name_artifacts_without_selection_state() {
        let dir = tempdir().unwrap();
        let store = ProviderStore::new(dir.path());

        let (id1, artifact1) = artifact("github");
        store.retain(&artifact1).unwrap();

        let index = store.read_index().unwrap();
        assert_eq!(index.version, 2);
        assert_eq!(index.providers.len(), 1);

        let (id2, artifact2) = {
            let metadata = serde_json::json!({
                "id": "github",
                "displayName": "github",
                "provider": "github-v2.wasm",
                "defaultMount": "github",
                "refreshIntervalSecs": 0
            });
            let bytes = wasm_with_provider_metadata(
                EMPTY_WASM,
                serde_json::to_vec(&metadata).unwrap().as_slice(),
            );
            let id = ProviderId::from_wasm_bytes(&bytes);
            (id, Artifact::from_bytes("github-v2.wasm", bytes).unwrap())
        };
        store.retain(&artifact2).unwrap();

        let index = store.read_index().unwrap();
        assert_eq!(index.providers.len(), 2);
        assert!(index.providers.iter().any(|entry| entry.id == id1));
        assert!(index.providers.iter().any(|entry| entry.id == id2));
    }

    #[test]
    fn read_index_rejects_unknown_version() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("index.json"),
            r#"{"version":1,"providers":[]}"#,
        )
        .unwrap();
        let store = ProviderStore::new(dir.path());
        assert!(matches!(store.read_index(), Err(StoreError::Version(1))));
    }

    #[cfg(unix)]
    #[test]
    fn store_rejects_preexisting_index_and_lock_symlinks() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let store = ProviderStore::new(dir.path());
        let index_target = dir.path().join("index-target.json");
        std::fs::write(&index_target, br#"{"version":2,"providers":[]}"#).unwrap();
        symlink(&index_target, dir.path().join(INDEX_FILE)).unwrap();
        assert!(matches!(
            store.read_index(),
            Err(StoreError::IndexNotRegular { .. })
        ));

        std::fs::remove_file(dir.path().join(INDEX_FILE)).unwrap();
        let lock_target = dir.path().join("lock-target");
        std::fs::write(&lock_target, b"").unwrap();
        symlink(&lock_target, dir.path().join(LOCK_FILE)).unwrap();
        let (_, artifact) = artifact("demo");
        assert!(matches!(
            store.retain(&artifact),
            Err(StoreError::LockNotRegular { .. })
        ));
        assert!(!store.artifact_path(&artifact.id).exists());
    }

    #[test]
    fn read_index_rejects_present_v2_without_providers() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("index.json"), r#"{"version":2}"#).unwrap();
        let store = ProviderStore::new(dir.path());
        assert!(matches!(store.read_index(), Err(StoreError::Index { .. })));
    }

    #[test]
    fn read_index_rejects_unknown_top_level_key() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("index.json"),
            r#"{"version":2,"providers":[],"providersByName":{}}"#,
        )
        .unwrap();
        let store = ProviderStore::new(dir.path());

        let error = store
            .read_index()
            .expect_err("unknown index keys must fail");

        let StoreError::Index { source, .. } = error else {
            panic!("expected corrupt index error");
        };
        let message = source.to_string();
        assert!(
            message.contains("unknown field `providersByName`"),
            "error should name the unknown key, got: {message}"
        );
    }

    #[test]
    fn read_index_rejects_unknown_provider_entry_key() {
        let dir = tempdir().unwrap();
        let id = ProviderId::from_wasm_bytes(b"demo").to_string();
        std::fs::write(
            dir.path().join("index.json"),
            format!(
                r#"{{"version":2,"providers":[{{"id":"{id}","name":"demo","source":"manual"}}]}}"#
            ),
        )
        .unwrap();
        let store = ProviderStore::new(dir.path());

        let error = store
            .read_index()
            .expect_err("unknown entry keys must fail");

        let StoreError::Index { source, .. } = error else {
            panic!("expected corrupt index error");
        };
        let message = source.to_string();
        assert!(
            message.contains("unknown field `source`"),
            "error should name the unknown key, got: {message}"
        );
    }

    #[test]
    fn read_index_rejects_duplicate_ids_and_invalid_names() {
        let dir = tempdir().unwrap();
        let id = ProviderId::from_wasm_bytes(b"demo").to_string();
        std::fs::write(
            dir.path().join("index.json"),
            format!(
                r#"{{"version":2,"providers":[{{"id":"{id}","name":"bad name"}},{{"id":"{id}","name":"demo"}}]}}"#
            ),
        )
        .unwrap();
        let store = ProviderStore::new(dir.path());

        let error = store
            .read_index()
            .expect_err("invalid provider names must fail during JSON deserialization");
        let StoreError::Index { source, .. } = error else {
            panic!("expected corrupt index error");
        };
        assert!(
            source
                .to_string()
                .contains("invalid provider_name `bad name`"),
            "error should name the invalid provider name, got: {source}"
        );

        std::fs::write(
            dir.path().join("index.json"),
            format!(
                r#"{{"version":2,"providers":[{{"id":"{id}","name":"demo"}},{{"id":"{id}","name":"demo"}}]}}"#
            ),
        )
        .unwrap();
        assert!(matches!(
            store.read_index(),
            Err(StoreError::DuplicateId(duplicate)) if duplicate.to_string() == id
        ));
    }

    #[test]
    fn retain_rejects_corrupt_collision_without_indexing_it() {
        let dir = tempdir().unwrap();
        let store = ProviderStore::new(dir.path());
        let (id, artifact) = artifact("demo");
        std::fs::write(store.artifact_path(&id), b"corrupt").unwrap();

        assert!(matches!(
            store.retain(&artifact),
            Err(StoreError::ArtifactMismatch { expected, .. }) if expected == id
        ));
        assert!(store.read_index().unwrap().providers.is_empty());
    }

    #[test]
    fn retain_rejects_non_file_collision_without_indexing_it() {
        let dir = tempdir().unwrap();
        let store = ProviderStore::new(dir.path());
        let (id, artifact) = artifact("demo");
        std::fs::create_dir(store.artifact_path(&id)).unwrap();

        assert!(matches!(
            store.retain(&artifact),
            Err(StoreError::ArtifactNotRegular { .. })
        ));
        assert!(store.read_index().unwrap().providers.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn retain_rejects_symlink_collision_without_indexing_it() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let store = ProviderStore::new(dir.path());
        let (id, artifact) = artifact("demo");
        let target = dir.path().join("target.wasm");
        std::fs::write(&target, &artifact.bytes).unwrap();
        symlink(&target, store.artifact_path(&id)).unwrap();

        assert!(matches!(
            store.retain(&artifact),
            Err(StoreError::ArtifactNotRegular { .. })
        ));
        assert!(store.read_index().unwrap().providers.is_empty());
    }

    #[test]
    fn concurrent_retain_of_same_artifact_is_idempotent() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let (_, source) = artifact("demo");
        let bytes = source.bytes.clone();

        let inserted = std::thread::scope(|scope| {
            let handles = (0..8)
                .map(|_| {
                    let root = root.clone();
                    let bytes = bytes.clone();
                    scope.spawn(move || {
                        let artifact = Artifact::from_bytes("demo.wasm", bytes).unwrap();
                        ProviderStore::new(root).retain(&artifact)
                    })
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap().unwrap())
                .collect::<Vec<_>>()
        });

        let store = ProviderStore::new(root);
        assert_eq!(store.read_index().unwrap().providers.len(), 1);
        assert_eq!(inserted.into_iter().filter(|inserted| *inserted).count(), 1);
    }
}
