//! Disk-backed blob cache for host-resident provider payloads.
//!
//! A request reference is the only durable selector. The response body is
//! published first under its content digest, then the reference atomically
//! selects that body and its response metadata. Providers never name either
//! filesystem entry.

use crate::cache::identity::{BlobGeneration, BlobRequestId};
use crate::sandbox::publish;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::Mutex as AsyncMutex;
use tracing::warn;

pub(crate) const BLOB_TMP_DIR: &str = ".tmp";
const OBJECTS_DIR: &str = "objects";
const REFS_DIR: &str = "refs";

#[derive(Debug, Clone)]
pub struct BlobRecord {
    /// Runtime-local blob id exposed through WIT as `blob-id`.
    pub id: u64,
    pub(crate) generation: BlobGeneration,
    pub size: u64,
    pub content_type: Option<String>,
    pub etag: Option<String>,
    pub status: u16,
    pub response_headers: Vec<(String, String)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct BlobMetadata {
    pub status: u16,
    pub content_type: Option<String>,
    pub etag: Option<String>,
    pub response_headers: Vec<(String, String)>,
    pub size: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct BlobRef {
    request_id: String,
    generation: String,
    #[serde(flatten)]
    metadata: BlobMetadata,
}

/// Durable blob bodies and atomic request references for one mount.
pub struct BlobCache {
    cache_dir: PathBuf,
    requests: DashMap<BlobRequestId, u64>,
    blobs: DashMap<u64, Arc<BlobRecord>>,
    locks: DashMap<BlobRequestId, Arc<AsyncMutex<()>>>,
    next_id: AtomicU64,
}

impl BlobCache {
    pub fn new(cache_dir: PathBuf) -> anyhow::Result<Self> {
        let cache_dir = Self::canonical_root(&cache_dir)?;
        let cache = Self {
            cache_dir,
            requests: DashMap::new(),
            blobs: DashMap::new(),
            locks: DashMap::new(),
            next_id: AtomicU64::new(1),
        };
        cache.prepare_dirs()?;
        cache.rehydrate()?;
        Ok(cache)
    }

    pub(crate) fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    pub(crate) fn canonical_root(path: &Path) -> std::io::Result<PathBuf> {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };
        let mut requested = PathBuf::new();
        for component in absolute.components() {
            match component {
                Component::CurDir => {},
                Component::ParentDir => {
                    requested.pop();
                },
                other => requested.push(other.as_os_str()),
            }
        }
        let mut existing = requested.clone();
        let mut missing = Vec::<OsString>::new();

        loop {
            match std::fs::symlink_metadata(&existing) {
                Ok(metadata) => {
                    if missing.is_empty() && metadata.file_type().is_symlink() {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "cache root is a symlink",
                        ));
                    }
                    let canonical_existing = std::fs::canonicalize(&existing)?;
                    if !std::fs::metadata(&canonical_existing)?.is_dir() {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::NotADirectory,
                            "cache root is not a directory",
                        ));
                    }
                    let mut canonical = canonical_existing;
                    for component in missing.iter().rev() {
                        canonical.push(component);
                    }
                    return Ok(canonical);
                },
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    let Some(name) = existing.file_name() else {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "cache root has no existing parent",
                        ));
                    };
                    missing.push(name.to_os_string());
                    existing.pop();
                },
                Err(error) => return Err(error),
            }
        }
    }

    fn prepare_dirs(&self) -> Result<(), BlobCacheError> {
        ensure_directory(&self.cache_dir)?;
        ensure_directory(&self.cache_dir.join(OBJECTS_DIR))?;
        ensure_directory(&self.cache_dir.join(REFS_DIR))?;

        let tmp = self.cache_dir.join(BLOB_TMP_DIR);
        match std::fs::symlink_metadata(&tmp) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(BlobCacheError::Internal(
                    "blob temporary root is a symlink".to_string(),
                ));
            },
            Ok(metadata) if metadata.is_dir() => {},
            Ok(_) => {},
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
            Err(error) => return Err(error.into()),
        }
        ensure_directory(&tmp)
    }

    pub fn lookup_by_id(&self, blob_id: u64) -> Option<Arc<BlobRecord>> {
        self.blobs.get(&blob_id).map(|entry| entry.clone())
    }

    pub(crate) fn lookup_by_request(&self, request_id: BlobRequestId) -> Option<Arc<BlobRecord>> {
        let id = self.requests.get(&request_id).map(|entry| *entry)?;
        self.lookup_by_id(id)
    }

    fn generation_path(&self, generation: BlobGeneration) -> PathBuf {
        self.cache_dir
            .join(OBJECTS_DIR)
            .join(generation.filesystem_name())
    }

    pub(crate) fn body_path(&self, record: &BlobRecord) -> PathBuf {
        self.generation_path(record.generation)
    }

    fn ref_path(&self, request_id: BlobRequestId) -> PathBuf {
        self.cache_dir
            .join(REFS_DIR)
            .join(format!("{}.json", request_id.filesystem_name()))
    }

    pub(crate) fn request_lock(&self, request_id: BlobRequestId) -> Arc<AsyncMutex<()>> {
        self.locks
            .entry(request_id)
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }

    /// Publish the body before replacing the request reference. A visible
    /// reference therefore always selects a complete body and matching size.
    pub(crate) fn publish(
        &self,
        request_id: BlobRequestId,
        generation: BlobGeneration,
        staged: &Path,
        metadata: BlobMetadata,
    ) -> Result<Arc<BlobRecord>, BlobCacheError> {
        let body_path = self.generation_path(generation);
        ensure_directory(body_path.parent().ok_or_else(|| {
            BlobCacheError::Internal("blob body has no parent directory".to_string())
        })?)?;

        match std::fs::symlink_metadata(&body_path) {
            Ok(existing) if existing.file_type().is_symlink() => {
                return Err(BlobCacheError::Internal(
                    "blob body is a symlink".to_string(),
                ));
            },
            Ok(_) if valid_body(&body_path, generation, metadata.size) => {
                remove_staged(staged)?;
            },
            Ok(_) => return Err(BlobCacheError::Internal("blob body is invalid".to_string())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::rename(staged, &body_path)?;
            },
            Err(error) => return Err(error.into()),
        }

        let blob_ref = BlobRef {
            request_id: request_id.filesystem_name(),
            generation: generation.filesystem_name(),
            metadata: metadata.clone(),
        };
        let json = serde_json::to_vec(&blob_ref)
            .map_err(|error| BlobCacheError::Internal(format!("serialize blob ref: {error}")))?;
        let ref_path = self.ref_path(request_id);
        ensure_directory(ref_path.parent().ok_or_else(|| {
            BlobCacheError::Internal("blob reference has no parent directory".to_string())
        })?)?;
        publish::replace_file_via_temp_rename(&ref_path, &json)?;
        Ok(self.store_published(request_id, generation, metadata))
    }

    fn store_published(
        &self,
        request_id: BlobRequestId,
        generation: BlobGeneration,
        metadata: BlobMetadata,
    ) -> Arc<BlobRecord> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let record = Arc::new(BlobRecord {
            id,
            generation,
            size: metadata.size,
            content_type: metadata.content_type,
            etag: metadata.etag,
            status: metadata.status,
            response_headers: metadata.response_headers,
        });
        self.blobs.insert(id, record.clone());
        self.requests.insert(request_id, id);
        record
    }

    fn rehydrate(&self) -> Result<(), BlobCacheError> {
        let refs = self.cache_dir.join(REFS_DIR);
        let Ok(refs_meta) = std::fs::symlink_metadata(&refs) else {
            return Ok(());
        };
        if !refs_meta.is_dir() || refs_meta.file_type().is_symlink() {
            return Err(BlobCacheError::Internal(
                "blob reference root is not an owned directory".to_string(),
            ));
        }
        let entries = std::fs::read_dir(refs)?;
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            let Ok(entry_meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if entry_meta.file_type().is_symlink() {
                return Err(BlobCacheError::Internal(
                    "blob reference entry is a symlink".to_string(),
                ));
            }
            if !entry_meta.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(stem) = name.strip_suffix(".json") else {
                continue;
            };
            let Some(request_id) = BlobRequestId::from_hex(stem) else {
                continue;
            };
            let Ok(raw) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(blob_ref) = serde_json::from_str::<BlobRef>(&raw) else {
                continue;
            };
            if blob_ref.request_id != request_id.filesystem_name() {
                continue;
            }
            let Some(generation) = BlobGeneration::from_hex(&blob_ref.generation) else {
                continue;
            };
            let body = self.generation_path(generation);
            if !valid_body(&body, generation, blob_ref.metadata.size) {
                warn!(path = %body.display(), "skipping invalid blob body");
                continue;
            }
            let _ = self.store_published(request_id, generation, blob_ref.metadata);
        }
        Ok(())
    }
}

fn ensure_directory(path: &Path) -> Result<(), BlobCacheError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {},
            Ok(_) => {
                return Err(BlobCacheError::Internal(
                    "blob cache path is not an owned directory".to_string(),
                ));
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&current)?;
            },
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn remove_staged(path: &Path) -> Result<(), BlobCacheError> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(BlobCacheError::Internal(
            "staged blob is not an owned regular file".to_string(),
        ));
    }
    std::fs::remove_file(path)?;
    Ok(())
}

fn valid_body(path: &Path, generation: BlobGeneration, expected_size: u64) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() != expected_size {
        return false;
    }
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0u8; 16 * 1024];
    loop {
        use std::io::Read;
        let Ok(read) = file.read(&mut buffer) else {
            return false;
        };
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    BlobGeneration::from_hash(hasher.finalize()) == generation
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum BlobCacheError {
    #[error("blob cache I/O failed")]
    Io(#[source] std::io::Error),
    #[error("internal: {0}")]
    Internal(String),
}

impl From<std::io::Error> for BlobCacheError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

#[cfg(test)]
mod tests {
    use super::{BlobCache, OBJECTS_DIR};

    #[test]
    fn cache_root_failure_is_typed_without_host_path() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("blob-cache");
        std::fs::write(&root, b"not a directory").unwrap();

        let error = match BlobCache::new(root.clone()) {
            Ok(_) => panic!("file cache root must fail closed"),
            Err(error) => error,
        };
        assert!(!error.to_string().contains(root.to_string_lossy().as_ref()));
        assert!(error.to_string().contains("directory"));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_cache_root_fails_closed_without_host_path() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("blob-cache");
        std::os::unix::fs::symlink(temp.path(), &root).unwrap();

        let error = match BlobCache::new(root.clone()) {
            Ok(_) => panic!("symlinked cache root must fail closed"),
            Err(error) => error,
        };
        assert!(!error.to_string().contains(root.to_string_lossy().as_ref()));
        assert!(error.to_string().contains("symlink"));
    }

    #[cfg(unix)]
    #[test]
    fn cache_root_canonicalizes_a_symlinked_existing_parent() {
        let temp = tempfile::tempdir().unwrap();
        let real_parent = temp.path().join("real");
        std::fs::create_dir(&real_parent).unwrap();
        let linked_parent = temp.path().join("linked");
        std::os::unix::fs::symlink(&real_parent, &linked_parent).unwrap();

        let requested = linked_parent.join("blob-cache");
        let cache = BlobCache::new(requested).unwrap();

        assert_eq!(
            cache.cache_dir(),
            std::fs::canonicalize(real_parent)
                .unwrap()
                .join("blob-cache")
        );
        assert!(cache.cache_dir().join(OBJECTS_DIR).is_dir());
    }
}
