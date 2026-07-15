//! Global content-addressed storage for durable provider bodies.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// BLAKE3 identity of one durable body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct BodyId([u8; 32]);

impl BodyId {
    #[must_use]
    pub(crate) fn from_bytes(bytes: &[u8]) -> Self {
        Self(*blake3::hash(bytes).as_bytes())
    }

    #[must_use]
    pub(crate) fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    #[must_use]
    pub(crate) fn hex(self) -> String {
        hex::encode(self.0)
    }
}

/// One append-only body store shared by all projections.
#[derive(Debug)]
pub(crate) struct BodyStore {
    root: PathBuf,
}

impl BodyStore {
    pub(crate) fn open(root: impl AsRef<Path>) -> Result<Self, BodyStoreError> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root)?;
        if fs::symlink_metadata(&root)?.file_type().is_symlink() {
            return Err(BodyStoreError::SymlinkRoot);
        }
        for entry in fs::read_dir(&root)? {
            let path = entry?.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if name.starts_with(".body-") && name.ends_with(".tmp") {
                fs::remove_file(path)?;
            }
        }
        Ok(Self { root })
    }

    /// Publish bytes before any projection can reference their returned id.
    /// Existing content is immutable and therefore treated as an idempotent
    /// successful publication after the temporary file is removed.
    pub(crate) fn publish(&self, bytes: &[u8]) -> Result<BodyId, BodyStoreError> {
        let id = BodyId::from_bytes(bytes);
        let destination = self.path(id);
        match fs::symlink_metadata(&destination) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(BodyStoreError::Destination { id });
            },
            Ok(_) => {
                self.verify_file(&destination, id, bytes.len() as u64)?;
                return Ok(id);
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => {},
            Err(error) => return Err(error.into()),
        }

        let temporary = self.temporary_path();
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            drop(file);
            self.verify_file(&temporary, id, bytes.len() as u64)?;
            match fs::hard_link(&temporary, &destination) {
                Ok(()) => {
                    fs::remove_file(&temporary)?;
                    sync_directory(&self.root)?;
                    Ok(id)
                },
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    let winner = self.verify_destination(&destination, id, bytes.len() as u64);
                    let removed = fs::remove_file(&temporary);
                    let synced = sync_directory(&self.root);
                    winner?;
                    removed?;
                    synced?;
                    Ok(id)
                },
                Err(error) => Err(error.into()),
            }
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    pub(crate) fn read(&self, id: BodyId) -> Result<Vec<u8>, BodyStoreError> {
        let path = self.path(id);
        let mut file = open_nofollow(&path)?;
        let metadata = file.metadata()?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(BodyStoreError::Destination { id });
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.read_to_end(&mut bytes)?;
        if BodyId::from_bytes(&bytes) != id {
            return Err(BodyStoreError::Digest { id });
        }
        Ok(bytes)
    }

    #[must_use]
    pub(crate) fn path(&self, id: BodyId) -> PathBuf {
        self.root.join(id.hex())
    }

    fn temporary_path(&self) -> PathBuf {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        self.root
            .join(format!(".body-{}-{sequence}.tmp", std::process::id()))
    }

    fn verify_file(
        &self,
        path: &Path,
        id: BodyId,
        expected_len: u64,
    ) -> Result<(), BodyStoreError> {
        let metadata = fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(BodyStoreError::Destination { id });
        }
        let mut file = open_nofollow(path)?;
        if metadata.len() != expected_len {
            return Err(BodyStoreError::Length {
                id,
                expected: expected_len,
                actual: metadata.len(),
            });
        }
        let mut hasher = blake3::Hasher::new();
        let mut length = 0_u64;
        let mut buffer = [0_u8; 8192];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            length += read as u64;
            hasher.update(&buffer[..read]);
        }
        if length != expected_len || hasher.finalize() != blake3::Hash::from_bytes(*id.as_bytes()) {
            return Err(BodyStoreError::Digest { id });
        }
        Ok(())
    }

    fn verify_destination(
        &self,
        path: &Path,
        id: BodyId,
        expected_len: u64,
    ) -> Result<(), BodyStoreError> {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                Err(BodyStoreError::Destination { id })
            },
            Ok(_) => self.verify_file(path, id, expected_len),
            Err(error) => Err(error.into()),
        }
    }
}

fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

fn open_nofollow(path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        return OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path);
    }
    #[cfg(not(unix))]
    {
        File::open(path)
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum BodyStoreError {
    #[error("body store I/O failed")]
    Io(#[source] io::Error),
    #[error("body store root must not be a symlink")]
    SymlinkRoot,
    #[error("body {id} has length {actual}, expected {expected}")]
    Length {
        id: BodyId,
        expected: u64,
        actual: u64,
    },
    #[error("body {id} failed digest validation")]
    Digest { id: BodyId },
    #[error("body {id} destination is not a regular file")]
    Destination { id: BodyId },
}

impl From<io::Error> for BodyStoreError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl std::fmt::Display for BodyId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.hex())
    }
}

#[cfg(test)]
mod tests {
    use super::BodyStore;

    #[test]
    fn publish_reopens_and_verifies_content_addressed_body() {
        let temp = tempfile::tempdir().unwrap();
        let store = BodyStore::open(temp.path()).unwrap();
        let id = store.publish(b"body").unwrap();
        assert_eq!(store.read(id).unwrap(), b"body");
        assert_eq!(store.publish(b"body").unwrap(), id);
        std::fs::write(store.path(id), b"corrupt").unwrap();
        assert!(matches!(
            store.publish(b"body"),
            Err(super::BodyStoreError::Length { .. } | super::BodyStoreError::Digest { .. })
        ));
        std::fs::remove_file(store.path(id)).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("elsewhere", store.path(id)).unwrap();
            assert!(matches!(
                store.publish(b"body"),
                Err(super::BodyStoreError::Destination { .. })
            ));
            assert!(matches!(
                store.read(id),
                Err(super::BodyStoreError::Destination { .. })
            ));
        }
    }
}
