//! Global content-addressed storage for durable provider bodies.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) use crate::view::BodyId;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// One append-only body store shared by all projections.
#[derive(Debug)]
pub(crate) struct BodyStore {
    root: PathBuf,
}

impl BodyStore {
    pub(crate) fn open(root: impl AsRef<Path>) -> Result<Self, BodyStoreError> {
        let root = crate::cache::canonical_directory(root.as_ref()).map_err(|error| {
            if error.kind() == io::ErrorKind::InvalidInput {
                BodyStoreError::SymlinkRoot
            } else {
                BodyStoreError::Io(error)
            }
        })?;
        crate::cache::ensure_directory(&root).map_err(|error| {
            if error.kind() == io::ErrorKind::Other {
                BodyStoreError::SymlinkRoot
            } else {
                BodyStoreError::Io(error)
            }
        })?;
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

    /// Open the existing global body store without creating directories or
    /// sweeping abandoned publication files.
    pub(crate) fn open_existing(root: impl AsRef<Path>) -> Result<Self, BodyStoreError> {
        let root = crate::cache::existing_directory(root.as_ref()).map_err(|error| {
            if error.kind() == io::ErrorKind::InvalidInput {
                BodyStoreError::SymlinkRoot
            } else {
                BodyStoreError::Io(error)
            }
        })?;
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

    /// Create a streaming body staged in the global body directory.
    pub(crate) fn stage(&self) -> Result<BodyWriter, BodyStoreError> {
        let path = self.temporary_path();
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        Ok(BodyWriter {
            root: self.root.clone(),
            path,
            file: Some(file),
            hasher: blake3::Hasher::new(),
            length: 0,
            published: false,
        })
    }

    /// Publish a streamed body without materializing it in memory.
    pub(crate) fn publish_staged(&self, mut staged: BodyWriter) -> Result<BodyId, BodyStoreError> {
        staged
            .file
            .as_mut()
            .expect("staged body file exists")
            .flush()?;
        staged
            .file
            .as_ref()
            .expect("staged body file exists")
            .sync_all()?;
        let id = BodyId::from_digest_bytes(*staged.hasher.finalize().as_bytes());
        let expected_len = staged.length;
        let metadata = staged
            .file
            .as_ref()
            .expect("staged body file exists")
            .metadata()?;
        if !metadata.is_file() || metadata.len() != expected_len {
            return Err(BodyStoreError::Length {
                id,
                expected: expected_len,
                actual: metadata.len(),
            });
        }
        let destination = self.path(id);
        match fs::symlink_metadata(&destination) {
            Ok(existing) if existing.file_type().is_symlink() || !existing.is_file() => {
                return Err(BodyStoreError::Destination { id });
            },
            Ok(_) => {
                self.verify_destination(&destination, id, expected_len)?;
                drop(staged.file.take());
                fs::remove_file(&staged.path)?;
                sync_directory(&self.root)?;
                staged.published = true;
                Ok(id)
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match fs::hard_link(&staged.path, &destination) {
                    Ok(()) => {
                        drop(staged.file.take());
                        fs::remove_file(&staged.path)?;
                        sync_directory(&self.root)?;
                        staged.published = true;
                        Ok(id)
                    },
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                        self.verify_destination(&destination, id, expected_len)?;
                        drop(staged.file.take());
                        fs::remove_file(&staged.path)?;
                        sync_directory(&self.root)?;
                        staged.published = true;
                        Ok(id)
                    },
                    Err(error) => Err(error.into()),
                }
            },
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn read(
        &self,
        id: BodyId,
        expected_len: Option<u64>,
    ) -> Result<Vec<u8>, BodyStoreError> {
        let path = self.path(id);
        let path_metadata = fs::symlink_metadata(&path)?;
        if path_metadata.file_type().is_symlink() || !path_metadata.is_file() {
            return Err(BodyStoreError::Destination { id });
        }
        let mut file = open_nofollow(&path)?;
        let metadata = file.metadata()?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(BodyStoreError::Destination { id });
        }
        if expected_len.is_some_and(|expected| expected != metadata.len()) {
            return Err(BodyStoreError::Length {
                id,
                expected: expected_len.unwrap_or(metadata.len()),
                actual: metadata.len(),
            });
        }
        let mut bytes = expected_len
            .and_then(|length| usize::try_from(length).ok())
            .map_or_else(Vec::new, Vec::with_capacity);
        file.read_to_end(&mut bytes)?;
        if BodyId::from_bytes(&bytes) != id {
            return Err(BodyStoreError::Digest { id });
        }
        Ok(bytes)
    }

    pub(crate) fn validate(
        &self,
        id: BodyId,
        expected_len: Option<u64>,
    ) -> Result<(), BodyStoreError> {
        let metadata = fs::symlink_metadata(self.path(id))?;
        let length = metadata.len();
        if expected_len.is_some_and(|expected| expected != length) {
            return Err(BodyStoreError::Length {
                id,
                expected: expected_len.unwrap_or(length),
                actual: length,
            });
        }
        self.verify_file(&self.path(id), id, length)
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

/// A body staged by [`BodyStore::stage`]. It owns the temporary file until
/// publication, so a failed or abandoned stream cannot leave a cache body.
pub(crate) struct BodyWriter {
    root: PathBuf,
    path: PathBuf,
    file: Option<File>,
    hasher: blake3::Hasher,
    length: u64,
    published: bool,
}

impl Write for BodyWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let written = self
            .file
            .as_mut()
            .expect("staged body file exists")
            .write(bytes)?;
        self.hasher.update(&bytes[..written]);
        self.length = self
            .length
            .checked_add(written as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "body length overflow"))?;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.as_mut().expect("staged body file exists").flush()
    }
}

impl Drop for BodyWriter {
    fn drop(&mut self) {
        if !self.published {
            let _ = fs::remove_file(&self.path);
            let _ = sync_directory(&self.root);
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
        assert_eq!(store.read(id, None).unwrap(), b"body");
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
                store.read(id, None),
                Err(super::BodyStoreError::Destination { .. })
            ));
        }
    }
}
