//! Disk-backed blob cache for host-resident provider payloads.
//!
//! Provider HTTP fetches whose payload should never cross the WIT
//! boundary land here. The body is stored under the provider-supplied
//! `cache-key`, and a runtime-local `blob-id` handle indexes the
//! metadata for later reads and archive extraction.

use crate::sandbox::{publish, relative_key};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::Mutex as AsyncMutex;
use tracing::warn;

pub(crate) const BLOB_TMP_DIR: &str = ".tmp";
pub(crate) const BLOB_META_DIR: &str = ".meta";

/// Metadata for a blob resident in the host cache.
///
/// The host stores the bytes at `BlobCache::blob_path(cache_key)` and
/// hands this record back to the provider as fetch metadata.
#[derive(Debug, Clone)]
pub struct BlobRecord {
    /// Runtime-local blob id exposed through WIT as `blob-id`.
    pub id: u64,
    /// Stable cache key supplied by the provider.
    pub cache_key: String,
    /// Size of the cached blob in bytes.
    pub size: u64,
    /// Response `Content-Type`, when present.
    pub content_type: Option<String>,
    /// Response `ETag`, when present.
    pub etag: Option<String>,
    /// HTTP status returned by the upstream fetch.
    pub status: u16,
    /// Decoded response headers preserved for provider inspection.
    pub response_headers: Vec<(String, String)>,
}

/// Disk-backed blob store, scoped to a single provider runtime. Each
/// blob is identified by a provider-supplied `cache-key` and assigned
/// an in-memory `u64` id the WIT exposes as `blob-id`.
///
/// IDs and the in-memory key index are allocated at runtime and are
/// not persisted across host restarts. Blob bytes and metadata are
/// rehydrated from disk when the cache starts, so construction is
/// synchronous and proportional to the number of cached blob files.
pub struct BlobCache {
    cache_dir: PathBuf,
    keys: DashMap<String, u64>,
    blobs: DashMap<u64, Arc<BlobRecord>>,
    locks: DashMap<String, Arc<AsyncMutex<()>>>,
    next_id: AtomicU64,
}

impl BlobCache {
    /// Create a cache rooted at `cache_dir` and clear stale temporary
    /// fetch files from a previous host process.
    pub fn new(cache_dir: PathBuf) -> Self {
        let cache = Self {
            cache_dir,
            keys: DashMap::new(),
            blobs: DashMap::new(),
            locks: DashMap::new(),
            next_id: AtomicU64::new(1),
        };

        let tmp_dir = cache.cache_dir.join(BLOB_TMP_DIR);
        let _ = std::fs::remove_dir_all(&tmp_dir);
        let _ = std::fs::create_dir_all(&tmp_dir);
        cache.rehydrate();
        cache
    }

    pub(crate) fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// Look up cached metadata by runtime-local id.
    pub fn lookup_by_id(&self, blob_id: u64) -> Option<Arc<BlobRecord>> {
        self.blobs.get(&blob_id).map(|r| r.clone())
    }

    /// Look up cached metadata by provider cache key.
    pub fn lookup_by_key(&self, cache_key: &str) -> Option<Arc<BlobRecord>> {
        let id = self.keys.get(cache_key).map(|entry| *entry)?;
        self.blobs.get(&id).map(|entry| entry.clone())
    }

    /// Read the raw bytes of a cached blob. Test support for the tape recorder.
    #[doc(hidden)]
    pub fn bytes_for_tests(&self, blob_id: u64) -> Option<Vec<u8>> {
        let record = self.lookup_by_id(blob_id)?;
        std::fs::read(self.blob_path(&record.cache_key)).ok()
    }

    /// Insert bytes as a cached blob and return its runtime-local id, registering
    /// the same on-disk file and `BlobRecord` shape the real fetch path produces,
    /// so later `read-blob` / `open-archive` callouts against the id behave
    /// exactly as after a real fetch. Test support for the tape replayer.
    #[doc(hidden)]
    pub fn insert_for_tests(
        &self,
        cache_key: &str,
        bytes: &[u8],
        content_type: Option<String>,
        etag: Option<String>,
        status: u16,
        response_headers: Vec<(String, String)>,
    ) -> u64 {
        // Mirror `BlobExecutor::materialize`: write the body under `blob_path`,
        // persist the rehydration metadata, then register the in-memory record.
        let blob_path = self.blob_path(cache_key);
        if let Some(parent) = blob_path.parent() {
            std::fs::create_dir_all(parent).expect("create blob dir");
        }
        let metadata = BlobMetadata {
            status,
            content_type,
            etag,
            response_headers,
            size: bytes.len() as u64,
        };
        self.store_metadata(cache_key, &metadata)
            .expect("store blob metadata");
        std::fs::write(&blob_path, bytes).expect("write blob bytes");
        self.store(cache_key.to_string(), metadata).id
    }

    /// Return the filesystem path for a provider cache key.
    pub(crate) fn blob_path(&self, cache_key: &str) -> PathBuf {
        self.cache_dir.join(cache_key)
    }

    /// Return the JSON metadata path for a provider cache key.
    pub(crate) fn metadata_path(&self, cache_key: &str) -> PathBuf {
        self.cache_dir
            .join(BLOB_META_DIR)
            .join(format!("{cache_key}.json"))
    }

    /// Store a blob in-memory index, assigning a runtime-local id.
    pub(crate) fn store(&self, cache_key: String, metadata: BlobMetadata) -> Arc<BlobRecord> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let record = Arc::new(BlobRecord {
            id,
            cache_key,
            size: metadata.size,
            content_type: metadata.content_type,
            etag: metadata.etag,
            status: metadata.status,
            response_headers: metadata.response_headers,
        });
        self.blobs.insert(id, record.clone());
        self.keys.insert(record.cache_key.clone(), id);
        record
    }

    /// Load existing blob files from disk and populate the in-memory index.
    fn rehydrate(&self) {
        let mut dirs = vec![self.cache_dir.clone()];
        while let Some(dir) = dirs.pop() {
            let Ok(read_dir) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in read_dir.filter_map(Result::ok) {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().into_owned();
                if name == BLOB_TMP_DIR || name == BLOB_META_DIR {
                    continue;
                }
                let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                    continue;
                };
                if metadata.is_dir() {
                    dirs.push(path);
                    continue;
                }
                if !metadata.is_file() {
                    continue;
                }

                let Some(relative_key) = path.strip_prefix(&self.cache_dir).ok() else {
                    continue;
                };
                let cache_key = relative_key.to_string_lossy().into_owned();
                if cache_key.is_empty() {
                    continue;
                }
                if !is_safe_path_segment(&cache_key) {
                    warn!(
                        cache_key,
                        path = %path.display(),
                        "skipping unsafe rehydrated blob key"
                    );
                    continue;
                }

                let metadata = match self.rehydrate_metadata(&cache_key) {
                    Ok(metadata) => metadata,
                    Err(error) => {
                        warn!(
                            cache_key,
                            error = %error,
                            path = %path.display(),
                            "failed to rehydrate blob metadata"
                        );
                        continue;
                    },
                };
                let _ = self.store(cache_key, metadata);
            }
        }
    }

    /// Rehydrate blob metadata from disk state.
    fn rehydrate_metadata(&self, cache_key: &str) -> Result<BlobMetadata, std::io::Error> {
        let metadata_path = self.metadata_path(cache_key);
        let raw = std::fs::read_to_string(&metadata_path)?;
        serde_json::from_str::<BlobMetadata>(&raw).map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("parse blob metadata {}: {error}", metadata_path.display()),
            )
        })
    }

    /// Persist the metadata needed to rehydrate this blob on restart.
    pub(crate) fn store_metadata(
        &self,
        cache_key: &str,
        metadata: &BlobMetadata,
    ) -> Result<(), BlobCacheError> {
        let path = self.metadata_path(cache_key);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec_pretty(&metadata).map_err(|error| {
            BlobCacheError::Io(std::io::Error::other(format!(
                "serialize blob metadata: {error}"
            )))
        })?;
        publish::replace_file_via_temp_rename(&path, &json)?;
        Ok(())
    }

    pub(crate) fn key_lock(&self, key: &str) -> Arc<AsyncMutex<()>> {
        self.locks
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct BlobMetadata {
    pub status: u16,
    pub content_type: Option<String>,
    pub etag: Option<String>,
    pub response_headers: Vec<(String, String)>,
    pub size: u64,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum BlobCacheError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Validate that a cache-key or provider-scope is safe to use under the
/// blob cache root.
pub(crate) fn is_safe_path_segment(s: &str) -> bool {
    relative_key::is_safe_relative_key(s, |component| {
        component == BLOB_TMP_DIR || component == BLOB_META_DIR
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthManager;
    use crate::blob::{BlobExecutor, BlobLimits};
    use crate::capability::CapabilityChecker;
    use crate::http::HttpStack;
    use omnifs_caps::Allowlist;
    use omnifs_wit::provider::types as wit_types;

    #[test]
    fn insert_for_tests_round_trips_and_reads_through_executor() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Arc::new(BlobCache::new(tmp.path().to_path_buf()));
        let bytes: &[u8] = b"hello blob bytes";
        let id = cache.insert_for_tests(
            "pkg/foo.bin",
            bytes,
            Some("application/octet-stream".into()),
            Some("etag-1".into()),
            200,
            vec![("x-test".into(), "value".into())],
        );

        let record = cache.lookup_by_id(id).expect("record present");
        assert_eq!(record.cache_key, "pkg/foo.bin");
        assert_eq!(record.size, bytes.len() as u64);
        assert_eq!(cache.bytes_for_tests(id).as_deref(), Some(bytes));

        // The inserted blob reads back through the real executor exactly as
        // after a fetch, proving the on-disk naming and record registration match.
        let capability = CapabilityChecker::new(Allowlist {
            domains: Vec::new(),
            git_repos: Vec::new(),
            needs_git: false,
            unix_sockets: Vec::new(),
        });
        let http =
            Arc::new(HttpStack::new(Arc::new(AuthManager::none()), Arc::new(capability)).unwrap());
        let executor = BlobExecutor::new(http, cache.clone(), BlobLimits::default());
        match executor.read(&wit_types::ReadBlobRequest {
            blob: id,
            offset: 0,
            len: None,
        }) {
            wit_types::CalloutResult::BlobRead(read) => assert_eq!(read.as_slice(), bytes),
            other => panic!("expected BlobRead, got {other:?}"),
        }
    }
}
