//! Provider-facing blob cache and `fetch-blob` / `read-blob` executors.
//!
//! Provider HTTP fetches whose payload should never cross the WIT
//! boundary land here. The body is streamed to a disk file under the
//! provider's blob cache directory using the provider-supplied
//! `cache-key`, and a `blob-id` handle is returned. Other host-side
//! machinery (FUSE blob-backed reads, archive extraction) consumes the
//! file in place, so the bytes never round-trip back through the
//! provider.

use crate::auth::AuthManager;
use crate::runtime::capability::CapabilityChecker;
use crate::runtime::executor::{CalloutResponse, ErrorKind};
use crate::runtime::http_headers::{build_header_map, decode_response_headers};
use crate::runtime::sandbox::publish;
use dashmap::DashMap;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;

const FETCH_TIMEOUT: Duration = Duration::from_secs(120);
const DEFAULT_MAX_FETCH_BLOB_BYTES: u64 = 1024 * 1024 * 1024;
const DEFAULT_MAX_READ_BLOB_BYTES: u64 = 16 * 1024 * 1024;
const BLOB_TMP_DIR: &str = ".tmp";
const BLOB_META_DIR: &str = ".meta";

/// Host-side size limits for blob fetches and guest-visible reads.
///
/// Fetch limits bound how much data the host stores on disk. Read
/// limits bound how many bytes a single `read-blob` callout may copy
/// back into provider memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlobLimits {
    /// Maximum response-body bytes accepted by `fetch-blob`.
    pub max_fetch_blob_bytes: u64,
    /// Maximum bytes returned by one `read-blob` response.
    pub max_read_blob_bytes: u64,
}

impl Default for BlobLimits {
    fn default() -> Self {
        Self {
            max_fetch_blob_bytes: DEFAULT_MAX_FETCH_BLOB_BYTES,
            max_read_blob_bytes: DEFAULT_MAX_READ_BLOB_BYTES,
        }
    }
}

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

#[derive(Debug, Clone)]
pub(crate) struct BlobRecordDraft {
    pub cache_key: String,
    pub size: u64,
    pub content_type: Option<String>,
    pub etag: Option<String>,
    pub status: u16,
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

    /// Look up cached metadata by runtime-local id.
    pub fn lookup_by_id(&self, blob_id: u64) -> Option<Arc<BlobRecord>> {
        self.blobs.get(&blob_id).map(|r| r.clone())
    }

    /// Look up cached metadata by provider cache key.
    pub fn lookup_by_key(&self, cache_key: &str) -> Option<Arc<BlobRecord>> {
        let id = self.keys.get(cache_key).map(|entry| *entry)?;
        self.blobs.get(&id).map(|entry| entry.clone())
    }

    /// Return the filesystem path for a provider cache key.
    pub(crate) fn blob_path(&self, cache_key: &str) -> PathBuf {
        self.cache_dir.join(cache_key)
    }

    /// Store a blob in-memory index, assigning a runtime-local id.
    pub(crate) fn store(&self, draft: BlobRecordDraft) -> Arc<BlobRecord> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let record = Arc::new(BlobRecord {
            id,
            cache_key: draft.cache_key,
            size: draft.size,
            content_type: draft.content_type,
            etag: draft.etag,
            status: draft.status,
            response_headers: draft.response_headers,
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
                    tracing::warn!(
                        cache_key,
                        path = %path.display(),
                        "skipping unsafe rehydrated blob key"
                    );
                    continue;
                }

                let record = match self.rehydrate_record(&cache_key) {
                    Ok(record) => record,
                    Err(error) => {
                        tracing::warn!(
                            cache_key,
                            error = %error,
                            path = %path.display(),
                            "failed to rehydrate blob metadata"
                        );
                        continue;
                    },
                };
                let _ = self.store(record);
            }
        }
    }

    /// Rehydrate a blob metadata record from disk state.
    fn rehydrate_record(&self, cache_key: &str) -> Result<BlobRecordDraft, std::io::Error> {
        let sidecar_path = sidecar_path(&self.cache_dir, cache_key);
        let sidecar = match std::fs::read_to_string(&sidecar_path) {
            Ok(raw) => serde_json::from_str::<BlobMetadata>(&raw).map_err(|error| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("parse blob metadata {}: {error}", sidecar_path.display()),
                )
            })?,
            Err(error) => return Err(error),
        };
        Ok(BlobRecordDraft {
            cache_key: cache_key.to_owned(),
            size: sidecar.size,
            content_type: sidecar.content_type,
            etag: sidecar.etag,
            status: sidecar.status,
            response_headers: sidecar.response_headers,
        })
    }

    fn store_sidecar(&self, record: &BlobRecordDraft) -> Result<(), BlobError> {
        let sidecar = BlobMetadata {
            status: record.status,
            content_type: record.content_type.clone(),
            etag: record.etag.clone(),
            response_headers: record.response_headers.clone(),
            size: record.size,
        };
        let path = sidecar_path(&self.cache_dir, &record.cache_key);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec_pretty(&sidecar).map_err(|error| {
            BlobError::Io(std::io::Error::other(format!(
                "serialize blob metadata: {error}"
            )))
        })?;
        publish::replace_file_via_temp_rename(&path, &json)?;
        Ok(())
    }

    fn key_lock(&self, key: &str) -> Arc<AsyncMutex<()>> {
        self.locks
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }
}

fn sidecar_path(cache_root: &Path, cache_key: &str) -> PathBuf {
    cache_root
        .join(BLOB_META_DIR)
        .join(format!("{cache_key}.json"))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BlobMetadata {
    status: u16,
    content_type: Option<String>,
    etag: Option<String>,
    response_headers: Vec<(String, String)>,
    size: u64,
}

/// Errors raised while fetching, storing, or reading host-resident blobs.
#[derive(Debug, thiserror::Error)]
enum BlobError {
    #[error("{operation} exceeds host blob cap ({actual} > {max} bytes)")]
    TooLarge {
        operation: &'static str,
        max: u64,
        actual: u64,
    },
    #[error("network: {0}")]
    Network(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Validate that a cache-key or provider-scope is safe to use as a path
/// component. Mirrors the rules in `cloner::is_safe_cache_key`.
fn is_safe_path_segment(s: &str) -> bool {
    if s.is_empty() || s.starts_with('/') {
        return false;
    }
    if s.bytes().any(|b| b == 0) {
        return false;
    }
    for component in s.split('/') {
        if component.is_empty() || component == ".." || component == "." {
            return false;
        }
        if component == BLOB_TMP_DIR || component == BLOB_META_DIR {
            return false;
        }
    }
    true
}

/// Executes provider blob callouts against a host-owned disk cache.
pub struct BlobExecutor {
    client: reqwest::Client,
    auth: Arc<AuthManager>,
    capability: Arc<CapabilityChecker>,
    cache: Arc<BlobCache>,
    limits: BlobLimits,
}

impl BlobExecutor {
    /// Construct an executor with explicit host blob limits.
    pub fn new(
        auth: Arc<AuthManager>,
        capability: Arc<CapabilityChecker>,
        cache: Arc<BlobCache>,
        limits: BlobLimits,
    ) -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder()
            .user_agent("omnifs")
            .connect_timeout(Duration::from_secs(10))
            .timeout(FETCH_TIMEOUT)
            .build()?;
        Ok(Self {
            client,
            auth,
            capability,
            cache,
            limits,
        })
    }

    /// Fetch an HTTP response body into the blob cache.
    ///
    /// The body is streamed to a temporary file and atomically renamed
    /// into place once it has stayed within the configured fetch cap.
    pub async fn fetch_blob(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: Option<&[u8]>,
        cache_key: &str,
    ) -> CalloutResponse {
        if !is_safe_path_segment(cache_key) {
            return error(
                ErrorKind::InvalidInput,
                format!("cache key {cache_key} is unsafe"),
                false,
            );
        }
        if let Err(e) = self.capability.check_url(url) {
            return error(ErrorKind::Denied, e.to_string(), false);
        }

        // Coalesce concurrent fetches of the same key.
        let lock = self.cache.key_lock(cache_key);
        let _guard = lock.lock().await;

        // Fast path: another caller already populated the cache.
        if let Some(record) = self.cache.lookup_by_key(cache_key) {
            return CalloutResponse::BlobFetched((*record).clone());
        }

        // Resolve auth + headers.
        let auth_headers = self.auth.headers_for_url(url);
        if auth_headers.is_empty() && self.auth.requires_auth_for_url(url) {
            return error(
                ErrorKind::Denied,
                format!("no credentials for {url}"),
                false,
            );
        }
        let Ok(reqwest_method) = reqwest::Method::from_str(method) else {
            return error(
                ErrorKind::Denied,
                format!("unsupported HTTP method: {method}"),
                false,
            );
        };
        let header_map = match build_header_map(&auth_headers, headers) {
            Ok(h) => h,
            Err(message) => return error(ErrorKind::Internal, message, false),
        };

        let mut req = self.client.request(reqwest_method, url).headers(header_map);
        if let Some(body) = body {
            req = req.body(body.to_vec());
        }

        let response = match req.send().await {
            Ok(r) => r,
            Err(e) => return error(ErrorKind::Network, e.to_string(), true),
        };

        let status = response.status().as_u16();
        let response_headers = decode_response_headers(response.headers());
        let etag = lookup_header(&response_headers, "etag");
        let content_type = lookup_header(&response_headers, "content-type");

        // Persist to disk.
        let blob_path = self.cache.blob_path(cache_key);
        if let Some(parent) = blob_path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            return error(ErrorKind::Internal, format!("create blob dir: {e}"), false);
        }
        let staged = match stream_response_body(
            response,
            &self.cache.cache_dir,
            self.limits.max_fetch_blob_bytes,
        )
        .await
        {
            Ok(staged) => staged,
            Err(e) => return blob_error("fetch blob", e),
        };
        let size = staged.size;

        let record = BlobRecordDraft {
            cache_key: cache_key.to_string(),
            size,
            content_type,
            etag,
            status,
            response_headers,
        };
        if let Err(error) = self.cache.store_sidecar(&record) {
            return blob_error("store blob metadata", error);
        }
        if let Err(error) = staged.persist(&blob_path) {
            // Best-effort: a stranded sidecar is overwritten by the next fetch for this key.
            let _ = std::fs::remove_file(sidecar_path(&self.cache.cache_dir, cache_key));
            return blob_error("publish blob", error);
        }
        let record = self.cache.store(record);

        CalloutResponse::BlobFetched((*record).clone())
    }

    /// Read a range from a cached blob back into provider memory.
    ///
    /// `offset` may point past EOF, which returns an empty byte vector.
    /// The configured read cap applies to the number of bytes returned
    /// by this call, not to the absolute offset.
    pub fn read_blob(&self, blob_id: u64, offset: u64, len: Option<u32>) -> CalloutResponse {
        let Some(record) = self.cache.lookup_by_id(blob_id) else {
            return error(
                ErrorKind::NotFound,
                format!("blob {blob_id} not found"),
                false,
            );
        };
        let path = self.cache.blob_path(&record.cache_key);
        match read_range(&path, offset, len, self.limits.max_read_blob_bytes) {
            Ok(bytes) => CalloutResponse::BlobRead(bytes),
            Err(e) => blob_error("read blob", e),
        }
    }
}

struct StagedBlob {
    tmp: tempfile::NamedTempFile,
    size: u64,
}

impl StagedBlob {
    fn persist(self, path: &Path) -> Result<(), BlobError> {
        self.tmp
            .persist(path)
            .map_err(|error| BlobError::Io(error.into()))?;
        Ok(())
    }
}

async fn stream_response_body(
    response: reqwest::Response,
    cache_root: &Path,
    max_bytes: u64,
) -> Result<StagedBlob, BlobError> {
    if let Some(content_length) = response.content_length()
        && content_length > max_bytes
    {
        return Err(BlobError::TooLarge {
            operation: "fetch blob",
            max: max_bytes,
            actual: content_length,
        });
    }
    let cache_dir = cache_root.join(BLOB_TMP_DIR);
    tokio::fs::create_dir_all(&cache_dir).await?;
    async {
        let mut tmp = tempfile::Builder::new()
            .prefix("fetch-")
            .suffix(".tmp")
            .tempfile_in(cache_dir)
            .map_err(BlobError::Io)?;
        let mut stream = response.bytes_stream();
        let mut total = 0_u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| BlobError::Network(e.to_string()))?;
            let chunk_len = u64::try_from(chunk.len()).map_err(|_| BlobError::TooLarge {
                operation: "fetch blob",
                max: max_bytes,
                actual: u64::MAX,
            })?;
            total = total.checked_add(chunk_len).ok_or(BlobError::TooLarge {
                operation: "fetch blob",
                max: max_bytes,
                actual: u64::MAX,
            })?;
            if total > max_bytes {
                return Err(BlobError::TooLarge {
                    operation: "fetch blob",
                    max: max_bytes,
                    actual: total,
                });
            }
            tmp.as_file_mut().write_all(&chunk).map_err(BlobError::Io)?;
        }
        tmp.flush().map_err(BlobError::Io)?;
        Ok(StagedBlob { tmp, size: total })
    }
    .await
}

fn read_range(
    path: &Path,
    offset: u64,
    len: Option<u32>,
    max_bytes: u64,
) -> Result<Vec<u8>, BlobError> {
    use std::io::{Read, Seek, SeekFrom};
    let file_len = std::fs::metadata(path)?.len();
    let available = file_len.saturating_sub(offset);
    let bytes_to_read = match len {
        Some(n) => available.min(u64::from(n)),
        None => available,
    };
    if bytes_to_read > max_bytes {
        return Err(BlobError::TooLarge {
            operation: "read blob",
            max: max_bytes,
            actual: bytes_to_read,
        });
    }

    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut buf = Vec::new();
    let mut limited = (&mut file).take(bytes_to_read);
    limited.read_to_end(&mut buf)?;
    Ok(buf)
}

fn lookup_header(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.clone())
}

fn error(kind: ErrorKind, message: String, retryable: bool) -> CalloutResponse {
    CalloutResponse::Error {
        kind,
        message,
        retryable,
    }
}

fn blob_error(context: &str, blob_error: BlobError) -> CalloutResponse {
    match blob_error {
        e @ BlobError::TooLarge { .. } => error(ErrorKind::TooLarge, e.to_string(), false),
        e @ BlobError::Network(_) => error(ErrorKind::Network, e.to_string(), true),
        BlobError::Io(e) => error(ErrorKind::Internal, format!("{context}: {e}"), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthManager;
    use crate::runtime::capability::{CapabilityChecker, CapabilityGrants};

    #[test]
    fn safe_path_segment_rejects_traversal() {
        assert!(is_safe_path_segment("crates/serde-1.0.197.crate"));
        assert!(!is_safe_path_segment("../etc/passwd"));
        assert!(!is_safe_path_segment("/abs"));
        assert!(!is_safe_path_segment(""));
        assert!(!is_safe_path_segment("a//b"));
        assert!(!is_safe_path_segment("a/./b"));
        assert!(!is_safe_path_segment(".tmp/fetch"));
        assert!(!is_safe_path_segment(".meta/fetch"));
        assert_eq!(
            sidecar_path(Path::new("/cache"), "pkg/foo.bin"),
            Path::new("/cache").join(".meta/pkg/foo.bin.json")
        );
    }

    #[test]
    fn lookup_header_is_case_insensitive() {
        let headers = vec![("Content-Type".into(), "text/plain".into())];
        assert_eq!(
            lookup_header(&headers, "content-type"),
            Some("text/plain".into())
        );
    }

    #[test]
    fn read_range_enforces_explicit_len_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("blob");
        std::fs::write(&path, b"abcdef").unwrap();

        assert_eq!(read_range(&path, 0, Some(4), 4).unwrap(), b"abcd".to_vec());
        assert!(matches!(
            read_range(&path, 0, Some(5), 4),
            Err(BlobError::TooLarge { .. })
        ));
        assert_eq!(read_range(&path, 3, Some(2), 4).unwrap(), b"de".to_vec());
        assert!(read_range(&path, 99, Some(4), 4).unwrap().is_empty());
    }

    #[test]
    fn read_range_enforces_read_to_end_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("blob");
        std::fs::write(&path, b"abcdef").unwrap();

        assert!(matches!(
            read_range(&path, 0, None, 4),
            Err(BlobError::TooLarge { .. })
        ));
        assert_eq!(read_range(&path, 2, None, 6).unwrap(), b"cdef".to_vec());
    }

    #[test]
    fn read_blob_maps_cap_to_too_large() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("blob");
        std::fs::write(&path, b"abcdef").unwrap();

        let cache = Arc::new(BlobCache::new(tmp.path().to_path_buf()));
        let record = cache.store(BlobRecordDraft {
            cache_key: "blob".into(),
            size: 6,
            content_type: None,
            etag: None,
            status: 200,
            response_headers: Vec::new(),
        });
        let capability = CapabilityChecker::new(CapabilityGrants {
            domains: Vec::new(),
            git_repos: Vec::new(),
            max_memory_mb: 16,
            needs_git: false,
        });
        let executor = BlobExecutor::new(
            Arc::new(AuthManager::none()),
            Arc::new(capability),
            cache,
            BlobLimits {
                max_fetch_blob_bytes: DEFAULT_MAX_FETCH_BLOB_BYTES,
                max_read_blob_bytes: 4,
            },
        )
        .unwrap();

        match executor.read_blob(record.id, 0, None) {
            CalloutResponse::Error {
                kind: ErrorKind::TooLarge,
                retryable: false,
                ..
            } => {},
            other => panic!("expected TooLarge read-blob error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn stream_response_body_rejects_large_content_length_before_writing() {
        let tmp = tempfile::tempdir().unwrap();
        let response: reqwest::Response = http::Response::builder()
            .header(http::header::CONTENT_LENGTH, "5")
            .body(reqwest::Body::from("abcde"))
            .unwrap()
            .into();

        let result = stream_response_body(response, tmp.path(), 4).await;

        assert!(matches!(result, Err(BlobError::TooLarge { actual: 5, .. })));
        assert!(!tmp.path().join(BLOB_TMP_DIR).exists());
    }

    #[tokio::test]
    async fn stream_response_body_rejects_body_that_exceeds_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let chunks = futures::stream::iter([
            Ok::<_, std::io::Error>(b"abc".to_vec()),
            Ok::<_, std::io::Error>(b"de".to_vec()),
        ]);
        let response: reqwest::Response = http::Response::builder()
            .body(reqwest::Body::wrap_stream(chunks))
            .unwrap()
            .into();

        let result = stream_response_body(response, tmp.path(), 4).await;

        assert!(matches!(result, Err(BlobError::TooLarge { actual: 5, .. })));
        assert!(
            std::fs::read_dir(tmp.path().join(BLOB_TMP_DIR))
                .unwrap()
                .next()
                .is_none()
        );
    }

    #[tokio::test]
    async fn stream_response_body_stages_until_explicit_publish() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("blob");
        let response: reqwest::Response = http::Response::builder()
            .body(reqwest::Body::from("abcde"))
            .unwrap()
            .into();

        let staged = stream_response_body(response, tmp.path(), 5)
            .await
            .expect("stage body");

        assert_eq!(staged.size, 5);
        assert!(!path.exists());
        staged.persist(&path).expect("publish staged body");
        assert_eq!(std::fs::read(&path).unwrap(), b"abcde");
    }

    #[test]
    fn rehydrates_existing_blob_after_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_root = tmp.path().join("blob-cache");
        let cache_key = "pkg-1.0/foo.bin";
        let blob_path = cache_root.join(cache_key);
        let sidecar = sidecar_path(&cache_root, cache_key);
        std::fs::create_dir_all(blob_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(sidecar.parent().unwrap()).unwrap();
        std::fs::write(&blob_path, b"hello world").unwrap();
        std::fs::write(
            &sidecar,
            serde_json::to_vec_pretty(&BlobMetadata {
                status: 200,
                content_type: Some("text/plain".into()),
                etag: Some("etag-1".into()),
                response_headers: vec![("x-test".into(), "value".into())],
                size: 11,
            })
            .unwrap(),
        )
        .unwrap();

        let cache = BlobCache::new(cache_root.clone());
        let record = cache
            .lookup_by_key(cache_key)
            .expect("expected rehydrated blob");

        assert_eq!(record.cache_key, cache_key);
        assert_eq!(cache.blob_path(&record.cache_key), blob_path);
        assert_eq!(record.size, 11);
        assert_eq!(record.status, 200);
        assert_eq!(record.content_type, Some("text/plain".to_string()));
        assert_eq!(record.etag, Some("etag-1".to_string()));
        assert_eq!(
            record.response_headers,
            vec![("x-test".into(), "value".into())]
        );
    }

    #[test]
    fn rehydrate_skips_blob_with_malformed_sidecar() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_root = tmp.path().join("blob-cache");
        let cache_key = "pkg-1.0/foo.bin";
        let blob_path = cache_root.join(cache_key);
        let sidecar = sidecar_path(&cache_root, cache_key);
        std::fs::create_dir_all(blob_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(sidecar.parent().unwrap()).unwrap();
        std::fs::write(&blob_path, b"hello world").unwrap();
        std::fs::write(&sidecar, b"{not json").unwrap();

        let cache = BlobCache::new(cache_root);

        assert!(cache.lookup_by_key(cache_key).is_none());
    }

    #[test]
    fn rehydrate_skips_blob_with_missing_sidecar() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_root = tmp.path().join("blob-cache");
        let cache_key = "old/path.bin";
        let blob_path = cache_root.join(cache_key);
        std::fs::create_dir_all(blob_path.parent().unwrap()).unwrap();
        std::fs::write(&blob_path, b"legacy").unwrap();

        let cache = BlobCache::new(cache_root);

        assert!(cache.lookup_by_key(cache_key).is_none());
    }
}
