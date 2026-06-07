//! JSON file-backed credential store.
//!
//! On-disk format: a JSON object `{ "version": 1, "entries": { ... } }`.
//! Writes are atomic: the payload is written through `atomic-write-file` and
//! committed into place. On Unix the file is mode 0600 after every write.

use crate::{CredentialEntry, CredentialStore, StoreError};
use atomic_write_file::OpenOptions as AtomicOpenOptions;
use fs2::FileExt;
use omnifs_core::CredentialId;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize)]
struct FileStoreData {
    version: u32,
    entries: BTreeMap<String, CredentialEntry>,
}

/// Credential store backed by a JSON file on disk.
///
/// The caller supplies the file path; this type does not resolve home
/// directories or create parent directories. The CLI resolves
/// `~/.omnifs/data/credentials.json` before constructing this type.
pub struct FileStore {
    path: PathBuf,
    lock_path: PathBuf,
}

impl FileStore {
    /// Creates a `FileStore` targeting `path`. The file is not created until
    /// the first `put` call.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let lock_path = default_lock_path(&path);
        Self { path, lock_path }
    }

    /// Creates a `FileStore` with an explicit lock path. Tests use this to
    /// keep the lock sentinel inside the temporary directory under test.
    pub fn with_lock_path(path: impl Into<PathBuf>, lock_path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            lock_path: lock_path.into(),
        }
    }

    fn load(&self) -> Result<FileStoreData, StoreError> {
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
            return Err(StoreError::Backend(format!(
                "unsupported file store version: {}",
                data.version
            )));
        }
        Ok(data)
    }

    fn save(&self, data: &FileStoreData) -> Result<(), StoreError> {
        if let Some(parent) = self.path.parent() {
            ensure_private_dir(parent)?;
        }
        let json = serde_json::to_vec_pretty(data)?;
        let mut options = AtomicOpenOptions::new();
        configure_private_file(&mut options);
        let mut file = options.open(&self.path)?;
        file.write_all(&json)?;
        file.commit()?;
        Ok(())
    }

    fn lock(&self) -> Result<File, StoreError> {
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
        update: impl FnOnce(&mut FileStoreData) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
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

fn default_lock_path(path: &Path) -> PathBuf {
    let mut lock = path.as_os_str().to_owned();
    lock.push(".lock");
    PathBuf::from(lock)
}

fn ensure_private_dir(path: &Path) -> Result<(), StoreError> {
    std::fs::create_dir_all(path)?;
    set_private_dir(path)
}

#[cfg_attr(not(unix), allow(unused_variables))]
fn configure_private_file(options: &mut AtomicOpenOptions) {
    #[cfg(unix)]
    {
        use atomic_write_file::unix::OpenOptionsExt as AtomicOpenOptionsExt;
        use std::os::unix::fs::OpenOptionsExt;

        options.preserve_mode(false).mode(0o600);
    }
}

/// Sets directory permissions to 0700 on Unix. No-op on other platforms.
#[cfg_attr(not(unix), allow(unused_variables))]
fn set_private_dir(path: &Path) -> Result<(), StoreError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o700);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(())
}

impl CredentialStore for FileStore {
    fn put(&self, key: &CredentialId, entry: &CredentialEntry) -> Result<(), StoreError> {
        self.update(|data| {
            data.entries.insert(key.storage_key(), entry.clone());
            Ok(())
        })
    }

    fn get(&self, key: &CredentialId) -> Result<Option<CredentialEntry>, StoreError> {
        let data = self.load()?;
        Ok(data.entries.get(&key.storage_key()).cloned())
    }

    fn delete(&self, key: &CredentialId) -> Result<(), StoreError> {
        self.update(|data| {
            data.entries.remove(&key.storage_key());
            Ok(())
        })
    }

    fn list(&self) -> Result<Option<Vec<CredentialId>>, StoreError> {
        let data = self.load()?;
        let keys = data
            .entries
            .keys()
            .map(|storage_key| storage_key.parse())
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)?;
        Ok(Some(keys))
    }

    fn backend_label(&self) -> String {
        format!("file store ({})", self.path.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CredentialStore;
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
            Err(StoreError::Backend(msg)) => {
                assert!(msg.contains("unsupported file store version: 99"), "{msg}");
            },
            other => panic!("expected Backend error, got {other:?}"),
        }
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
