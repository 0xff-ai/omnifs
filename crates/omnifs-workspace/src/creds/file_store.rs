//! JSON file-backed credential store.
//!
//! On-disk format: a JSON object `{ "version": 1, "entries": { ... } }`.
//! Writes are atomic: the payload is written through `atomic-write-file` and
//! committed into place. On Unix the file is mode 0600 after every write.

use crate::authn::CredentialId;
use crate::creds::{CredStoreError, CredentialEntry, CredentialStore};
use crate::io::{ensure_private_dir, write_atomic};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize)]
struct FileStoreData {
    version: u32,
    entries: BTreeMap<String, CredentialEntry>,
}

/// Credential store backed by a JSON file on disk.
///
/// The caller supplies the file path; this type does not resolve home
/// directories or create parent directories. The CLI resolves the concrete
/// credentials file path before constructing this type.
pub struct FileStore {
    path: PathBuf,
    lock_path: PathBuf,
}

impl FileStore {
    /// Creates a `FileStore` targeting `path`. The file is not created until
    /// the first `put` call.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let lock_path = Self::lock_path_for(&path);
        Self { path, lock_path }
    }

    fn lock_path_for(path: &Path) -> PathBuf {
        let mut lock = path.as_os_str().to_owned();
        lock.push(".lock");
        PathBuf::from(lock)
    }

    /// Creates a `FileStore` with an explicit lock path. Tests use this to
    /// keep the lock sentinel inside the temporary directory under test.
    #[cfg(test)]
    fn with_lock_path(path: impl Into<PathBuf>, lock_path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            lock_path: lock_path.into(),
        }
    }

    fn load(&self) -> Result<FileStoreData, CredStoreError> {
        let raw = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(FileStoreData {
                    version: 1,
                    entries: BTreeMap::new(),
                });
            },
            Err(e) => return Err(e.into()),
        };
        let data: FileStoreData = serde_json::from_slice(&raw)?;
        if data.version != 1 {
            return Err(CredStoreError::Backend(format!(
                "unsupported file store version: {}",
                data.version
            )));
        }
        Ok(data)
    }

    fn save(&self, data: &FileStoreData) -> Result<(), CredStoreError> {
        if let Some(parent) = self.path.parent() {
            ensure_private_dir(parent)?;
        }
        let json = serde_json::to_vec_pretty(data)?;
        write_atomic(&self.path, &json, 0o600)?;
        Ok(())
    }

    fn lock(&self) -> Result<File, CredStoreError> {
        if let Some(parent) = self.lock_path.parent() {
            ensure_private_dir(parent)?;
        }
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.lock_path)?;
        lock.lock_exclusive()?;
        Ok(lock)
    }

    fn update(
        &self,
        update: impl FnOnce(&mut FileStoreData) -> Result<(), CredStoreError>,
    ) -> Result<(), CredStoreError> {
        let lock = self.lock()?;
        let mut data = self.load()?;
        let result = update(&mut data).and_then(|()| self.save(&data));
        match (result, lock.unlock()) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(e), _) => Err(e),
            (Ok(()), Err(e)) => Err(e.into()),
        }
    }
}

impl CredentialStore for FileStore {
    fn put(&self, key: &CredentialId, entry: &CredentialEntry) -> Result<(), CredStoreError> {
        self.update(|data| {
            data.entries.insert(key.storage_key(), entry.clone());
            Ok(())
        })
    }

    fn get(&self, key: &CredentialId) -> Result<Option<CredentialEntry>, CredStoreError> {
        let data = self.load()?;
        Ok(data.entries.get(&key.storage_key()).cloned())
    }

    fn delete(&self, key: &CredentialId) -> Result<(), CredStoreError> {
        self.update(|data| {
            data.entries.remove(&key.storage_key());
            Ok(())
        })
    }

    fn list(&self) -> Result<Option<Vec<CredentialId>>, CredStoreError> {
        let data = self.load()?;
        let keys = data
            .entries
            .keys()
            .map(|storage_key| storage_key.parse())
            .collect::<Result<Vec<_>, _>>()
            .map_err(CredStoreError::from)?;
        Ok(Some(keys))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::creds::CredentialStore;
    use secrecy::{ExposeSecret, SecretString};
    use std::sync::{Arc, Barrier};
    use std::thread;
    use tempfile::tempdir;
    use time::OffsetDateTime;

    fn sample(value: &str) -> CredentialEntry {
        let mut entry = CredentialEntry::static_token(
            SecretString::from(value.to_string()),
            OffsetDateTime::UNIX_EPOCH,
        );
        entry.set_upstream_identity(Some("user@example.com".into()));
        entry
    }

    #[test]
    fn rejects_bad_version() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("creds.json");
        std::fs::write(&path, r#"{"version":99,"entries":{}}"#).unwrap();
        let store = FileStore::new(&path);
        match store.get(&CredentialId::new("x", "static-token", "default").unwrap()) {
            Err(CredStoreError::Backend(msg)) => {
                assert!(msg.contains("unsupported file store version: 99"), "{msg}");
            },
            other => panic!("expected Backend error, got {other:?}"),
        }
    }

    #[test]
    fn loads_minimal_static_token_entry() {
        // The dev credentials template (`contrib/dev-credentials.json`) writes
        // the minimal on-disk form: only the three required fields. Guard that
        // it round-trips so interpolating env values into that template yields a
        // store the host can read.
        let dir = tempdir().unwrap();
        let path = dir.path().join("creds.json");
        std::fs::write(
            &path,
            r#"{"version":1,"entries":{"github:pat:default":{"kind":"static-token","access_token":"ghp_x","stored_at":"1970-01-01T00:00:00Z"}}}"#,
        )
        .unwrap();
        let entry = FileStore::new(&path)
            .get(&CredentialId::new("github", "pat", "default").unwrap())
            .unwrap()
            .expect("minimal entry loads");
        assert_eq!(entry.access_token().expose_secret(), "ghp_x");
        assert_eq!(entry.token_type(), "Bearer");
    }

    #[test]
    fn survives_reopen() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let key = CredentialId::new("github", "user", "raul@example.com").unwrap();

        FileStore::new(&path)
            .put(&key, &sample("secret-value"))
            .unwrap();

        // A second instance over the same path should see the stored entry.
        let got = FileStore::new(&path)
            .get(&key)
            .unwrap()
            .expect("entry should persist across instances");
        assert_eq!(got.access_token().expose_secret(), "secret-value");
    }

    #[test]
    fn concurrent_writers_do_not_lose_entries() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let lock_path = dir.path().join("creds.lock");
        let writers = 12;
        let barrier = Arc::new(Barrier::new(writers));
        let mut handles = Vec::new();

        for idx in 0..writers {
            let path = path.clone();
            let lock_path = lock_path.clone();
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                let store = FileStore::with_lock_path(path, lock_path);
                let key = CredentialId::new(format!("provider-{idx}"), "static-token", "default")
                    .unwrap();
                barrier.wait();
                store.put(&key, &sample(&format!("secret-{idx}"))).unwrap();
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        let keys = FileStore::with_lock_path(&path, &lock_path)
            .list()
            .unwrap()
            .unwrap();
        assert_eq!(keys.len(), writers);
    }
}
