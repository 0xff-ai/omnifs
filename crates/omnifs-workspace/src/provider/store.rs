//! Content-addressed provider store.
//!
//! Layout under the store root (by default `~/.omnifs/providers/`):
//!
//! ```text
//! <64hex>.wasm   immutable provider artifacts, write-if-absent
//! index.json     name index + latest pointers
//! ```
//!
//! Artifacts are keyed by [`ProviderId`] (BLAKE3 of the exact WASM bytes), so a
//! present `<id>.wasm` is always the correct content. The CLI is the only writer;
//! `install` advances `latest[name]` under an advisory lock (same pattern as the
//! credentials `FileStore`) so two concurrent CLI processes do not lose an update
//! in the read-modify-write.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use crate::ids::{ProviderId, ProviderMeta, ProviderName, ProviderVersion};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::provider::Artifact;

const INDEX_FILE: &str = "index.json";
const LOCK_FILE: &str = ".index.lock";
const INDEX_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("provider store io error at {}: {source}", path.display())]
    Io { path: PathBuf, source: io::Error },
    #[error("provider index at {} is corrupt: {source}", path.display())]
    Index {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("unsupported provider index version {0}")]
    Version(u32),
}

/// One installed artifact in the index. `file` is display-only provenance.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexEntry {
    pub id: ProviderId,
    pub name: ProviderName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<ProviderVersion>,
    pub file: String,
}

/// The provider index: every retained artifact plus a name→latest-id pointer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Index {
    pub version: u32,
    #[serde(default)]
    pub providers: Vec<IndexEntry>,
    #[serde(default)]
    pub latest: BTreeMap<String, ProviderId>,
}

impl Index {
    #[must_use]
    fn empty() -> Self {
        Self {
            version: INDEX_VERSION,
            providers: Vec::new(),
            latest: BTreeMap::new(),
        }
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

    /// Write `bytes` under `<id>.wasm`, atomically, only if absent.
    /// Content addressing makes a present file always correct, so a hit is a skip.
    pub fn put_if_absent(&self, id: &ProviderId, bytes: &[u8]) -> Result<(), StoreError> {
        create_dir_all(&self.root)?;
        let final_path = self.artifact_path(id);
        if final_path.exists() {
            return Ok(());
        }
        let tmp = self
            .root
            .join(format!(".{id}.wasm.tmp-{}", std::process::id()));
        write_file(&tmp, bytes)?;
        fs::rename(&tmp, &final_path).map_err(|source| StoreError::Io {
            path: final_path,
            source,
        })
    }

    /// Retain one validated provider artifact and advance its provider-name
    /// latest pointer.
    pub fn add_artifact(&self, artifact: Artifact) -> Result<IndexEntry, StoreError> {
        let entry = IndexEntry {
            id: artifact.id,
            name: artifact.meta.name.clone(),
            version: artifact.meta.version.clone(),
            file: artifact.file.clone(),
        };
        self.put_if_absent(&artifact.id, &artifact.bytes)?;
        self.install(artifact.id, artifact.meta, artifact.file)?;
        Ok(entry)
    }

    /// Read the index, or an empty (version-stamped) one if it does not exist yet.
    pub fn read_index(&self) -> Result<Index, StoreError> {
        let path = self.index_path();
        match fs::read(&path) {
            Ok(bytes) => {
                let index: Index = serde_json::from_slice(&bytes)
                    .map_err(|source| StoreError::Index { path, source })?;
                if index.version != INDEX_VERSION {
                    return Err(StoreError::Version(index.version));
                }
                Ok(index)
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Index::empty()),
            Err(source) => Err(StoreError::Io { path, source }),
        }
    }

    /// Upsert one provider into the index and advance `latest[name]`, under an
    /// advisory lock on the store so concurrent CLI installs serialize.
    pub fn install(
        &self,
        id: ProviderId,
        meta: ProviderMeta,
        file: String,
    ) -> Result<(), StoreError> {
        create_dir_all(&self.root)?;
        let lock = self.lock()?;
        let result = self.install_locked(id, meta, file);
        // Release the lock regardless; an unlock failure must not mask the result.
        let _ = FileExt::unlock(&lock);
        result
    }

    fn install_locked(
        &self,
        id: ProviderId,
        meta: ProviderMeta,
        file: String,
    ) -> Result<(), StoreError> {
        let mut index = self.read_index()?;
        if !index.providers.iter().any(|entry| entry.id == id) {
            index.providers.push(IndexEntry {
                id,
                name: meta.name.clone(),
                version: meta.version,
                file,
            });
        }
        index.latest.insert(meta.name.to_string(), id);
        self.write_index(&index)
    }

    fn write_index(&self, index: &Index) -> Result<(), StoreError> {
        let path = self.index_path();
        let bytes = serde_json::to_vec_pretty(index).map_err(|source| StoreError::Index {
            path: path.clone(),
            source,
        })?;
        let tmp = self
            .root
            .join(format!("{INDEX_FILE}.tmp-{}", std::process::id()));
        write_file(&tmp, &bytes)?;
        fs::rename(&tmp, &path).map_err(|source| StoreError::Io { path, source })
    }

    fn lock(&self) -> Result<File, StoreError> {
        let path = self.root.join(LOCK_FILE);
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| StoreError::Io {
                path: path.clone(),
                source,
            })?;
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

fn write_file(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    fs::write(path, bytes).map_err(|source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Artifact;
    use crate::provider::sections::wasm_with_provider_metadata;
    use tempfile::tempdir;

    const EMPTY_WASM: &[u8] = b"\0asm\x01\0\0\0";

    fn meta(name: &str, version: Option<&str>) -> ProviderMeta {
        ProviderMeta {
            name: ProviderName::new(name).unwrap(),
            version: version.map(ProviderVersion::new),
        }
    }

    fn artifact(file: &str, name: &str) -> (ProviderId, Artifact) {
        let metadata = serde_json::json!({
            "id": name,
            "displayName": name,
            "provider": file,
            "defaultMount": name
        });
        let bytes = wasm_with_provider_metadata(
            EMPTY_WASM,
            serde_json::to_vec(&metadata).unwrap().as_slice(),
        );
        let id = ProviderId::from_wasm_bytes(&bytes);
        (id, Artifact::from_bytes(file, bytes).unwrap())
    }

    #[test]
    fn add_artifact_writes_root_hash_file_and_indexes_metadata() {
        let dir = tempdir().unwrap();
        let store = ProviderStore::new(dir.path());
        let (id, artifact) = artifact("omnifs_provider_demo.wasm", "demo");

        let entry = store.add_artifact(artifact).unwrap();

        assert_eq!(entry.id, id);
        assert_eq!(entry.name.as_str(), "demo");
        assert_eq!(entry.file, "omnifs_provider_demo.wasm");
        assert!(store.artifact_path(&id).is_file());
        assert!(!dir.path().join("by-hash").exists());
        let index = store.read_index().unwrap();
        assert_eq!(index.providers.len(), 1);
        assert_eq!(index.latest.get("demo"), Some(&id));
    }

    #[test]
    fn put_if_absent_is_content_addressed_and_idempotent() {
        let dir = tempdir().unwrap();
        let store = ProviderStore::new(dir.path());
        let bytes = b"provider wasm bytes";
        let id = ProviderId::from_wasm_bytes(bytes);

        store.put_if_absent(&id, bytes).unwrap();
        assert!(store.artifact_path(&id).exists());
        assert_eq!(std::fs::read(store.artifact_path(&id)).unwrap(), bytes);
        // Second call is a no-op skip (already present).
        store.put_if_absent(&id, bytes).unwrap();
    }

    #[test]
    fn install_records_entry_and_advances_latest() {
        let dir = tempdir().unwrap();
        let store = ProviderStore::new(dir.path());

        let v1 = b"github v1";
        let id1 = ProviderId::from_wasm_bytes(v1);
        store.put_if_absent(&id1, v1).unwrap();
        store
            .install(
                id1,
                meta("github", Some("0.3.0")),
                "omnifs_provider_github.wasm".into(),
            )
            .unwrap();

        let index = store.read_index().unwrap();
        assert_eq!(index.version, 1);
        assert_eq!(index.providers.len(), 1);
        assert_eq!(index.latest.get("github"), Some(&id1));

        // A newer artifact for the same name advances latest but retains both.
        let v2 = b"github v2";
        let id2 = ProviderId::from_wasm_bytes(v2);
        store.put_if_absent(&id2, v2).unwrap();
        store
            .install(
                id2,
                meta("github", Some("0.3.1")),
                "omnifs_provider_github.wasm".into(),
            )
            .unwrap();

        let index = store.read_index().unwrap();
        assert_eq!(index.providers.len(), 2);
        assert_eq!(index.latest.get("github"), Some(&id2));
    }

    #[test]
    fn read_index_rejects_unknown_version() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("index.json"), r#"{"version":99}"#).unwrap();
        let store = ProviderStore::new(dir.path());
        assert!(matches!(store.read_index(), Err(StoreError::Version(99))));
    }

    #[test]
    fn read_index_rejects_unknown_top_level_key() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("index.json"),
            r#"{"version":1,"providers":[],"latest":{},"providersByName":{}}"#,
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
                r#"{{"version":1,"providers":[{{"id":"{id}","name":"demo","file":"omnifs_provider_demo.wasm","source":"manual"}}]}}"#
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
}
