//! Provider-facing `fetch-blob` and `read-blob` executors.
//!
//! Provider HTTP fetches whose payload should never cross the WIT
//! boundary are streamed into [`crate::blob_cache::BlobCache`].
//! Other host-side machinery (FUSE blob-backed reads, archive
//! extraction) consumes the file in place, so the bytes never
//! round-trip back through the provider.

#[cfg(test)]
use crate::blob_cache::BLOB_META_DIR;
pub use crate::blob_cache::BlobCache;
use crate::blob_cache::{
    BLOB_TMP_DIR, BlobCacheError, BlobMetadata, BlobRecord, is_safe_path_segment,
};
use crate::callouts::{
    callout_internal, callout_invalid, callout_network, callout_not_found, callout_too_large,
    record_outcome,
};
use crate::http::{HttpStack, decode_response_headers};
use crate::log_redaction::{LogUrl, WitHeaders};
use futures::StreamExt;
use omnifs_wit::provider::types as wit_types;
use omnifs_workspace::mounts::Spec;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

const BLOB_FETCH_TIMEOUT: Duration = Duration::from_mins(2);

const DEFAULT_MAX_FETCH_BLOB_BYTES: u64 = 1024 * 1024 * 1024;
const DEFAULT_MAX_READ_BLOB_BYTES: u64 = 16 * 1024 * 1024;

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

impl BlobLimits {
    pub fn from_config(config: &Spec) -> Self {
        let defaults = Self::default();
        let limits = config.limits.as_ref();
        Self {
            max_fetch_blob_bytes: limits
                .and_then(|limits| limits.max_fetch_blob_bytes)
                .unwrap_or(defaults.max_fetch_blob_bytes),
            max_read_blob_bytes: limits
                .and_then(|limits| limits.max_read_blob_bytes)
                .unwrap_or(defaults.max_read_blob_bytes),
        }
    }
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
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Internal(String),
}

impl BlobError {
    /// Add operation context to an I/O error without relabeling typed failures.
    fn with_io_context(self, context: &'static str) -> Self {
        match self {
            Self::Io(error) => Self::Internal(format!("{context}: {error}")),
            other => other,
        }
    }
}

impl From<BlobCacheError> for BlobError {
    fn from(error: BlobCacheError) -> Self {
        match error {
            BlobCacheError::Io(error) => Self::Io(error),
        }
    }
}

impl From<BlobError> for wit_types::CalloutResult {
    fn from(error: BlobError) -> Self {
        match error {
            BlobError::TooLarge { .. } => callout_too_large(error.to_string()),
            BlobError::Network(_) => callout_network(error.to_string()),
            BlobError::Io(_) => callout_internal(error.to_string()),
            BlobError::NotFound(msg) => callout_not_found(msg),
            BlobError::Internal(msg) => callout_internal(msg),
        }
    }
}

/// Executes provider blob callouts against a host-owned disk cache.
#[derive(Clone)]
pub struct BlobExecutor {
    http: Arc<HttpStack>,
    cache: Arc<BlobCache>,
    limits: BlobLimits,
}

impl BlobExecutor {
    /// Construct an executor with explicit host blob limits.
    pub fn new(http: Arc<HttpStack>, cache: Arc<BlobCache>, limits: BlobLimits) -> Self {
        Self {
            http,
            cache,
            limits,
        }
    }

    /// Fetch an HTTP response body into the blob cache.
    ///
    /// The body is streamed to a temporary file and atomically renamed
    /// into place once it has stayed within the configured fetch cap.
    #[tracing::instrument(target = "omnifs_callout", skip_all, fields(
        cache_key = %req.cache_key,
        method = req.method.as_str(),
        url = %LogUrl(&req.url),
        request_headers = %WitHeaders(&req.headers),
        request_body_bytes = req.body.as_ref().map_or(0, Vec::len),
        blob = tracing::field::Empty,
        status = tracing::field::Empty,
        response_headers = tracing::field::Empty,
        response_body_bytes = tracing::field::Empty,
        error.kind = tracing::field::Empty,
        error.message = tracing::field::Empty,
        error.retryable = tracing::field::Empty,
    ))]
    pub async fn fetch(&self, req: &wit_types::BlobFetchRequest) -> wit_types::CalloutResult {
        let result = if is_safe_path_segment(&req.cache_key) {
            let lock = self.cache.key_lock(&req.cache_key);
            let _guard = lock.lock().await;
            if let Some(record) = self.cache.lookup_by_key(&req.cache_key) {
                wit_types::CalloutResult::BlobFetched(record.as_ref().into())
            } else {
                // HttpStack::send already returns a fully-formed CalloutResult on
                // pre-flight or network failure — pass it through unchanged.
                match self
                    .http
                    .send(
                        &req.method,
                        &req.url,
                        &req.headers,
                        req.body.as_deref(),
                        BLOB_FETCH_TIMEOUT,
                    )
                    .await
                {
                    Ok(response) => match self.materialize(&req.cache_key, response).await {
                        Ok(record) => wit_types::CalloutResult::BlobFetched(record.as_ref().into()),
                        Err(e) => e.into(),
                    },
                    Err(early) => early,
                }
            }
        } else {
            callout_invalid(format!("cache key {} is unsafe", req.cache_key))
        };
        record_outcome(&result);
        result
    }

    /// Stream the response body to disk and persist the cache record.
    /// Internal helper that keeps `BlobError` typed up to the public
    /// `fetch` boundary so the `?` operator and `From` impls stay in
    /// play here.
    async fn materialize(
        &self,
        cache_key: &str,
        response: reqwest::Response,
    ) -> Result<Arc<BlobRecord>, BlobError> {
        let status = response.status().as_u16();
        let response_headers = decode_response_headers(response.headers());
        let etag = lookup_header(&response_headers, "etag");
        let content_type = lookup_header(&response_headers, "content-type");

        let blob_path = self.cache.blob_path(cache_key);
        if let Some(parent) = blob_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| BlobError::Internal(format!("create blob dir: {e}")))?;
        }
        let staged = stream_response_body(
            response,
            self.cache.cache_dir(),
            self.limits.max_fetch_blob_bytes,
        )
        .await
        .map_err(|error| error.with_io_context("fetch blob"))?;
        let size = staged.size;

        let metadata = BlobMetadata {
            status,
            content_type,
            etag,
            response_headers,
            size,
        };
        self.cache
            .store_metadata(cache_key, &metadata)
            .map_err(|error| BlobError::from(error).with_io_context("store blob metadata"))?;
        if let Err(error) = staged.persist(&blob_path) {
            // Best-effort: stranded metadata is overwritten by the next fetch for this key.
            let _ = std::fs::remove_file(self.cache.metadata_path(cache_key));
            return Err(error.with_io_context("publish blob"));
        }
        Ok(self.cache.store(cache_key.to_string(), metadata))
    }

    /// Read a range from a cached blob back into provider memory.
    ///
    /// `offset` may point past EOF, which returns an empty byte vector.
    /// The configured read cap applies to the number of bytes returned
    /// by this call, not to the absolute offset.
    #[tracing::instrument(target = "omnifs_callout", skip_all, fields(
        blob = req.blob,
        offset = req.offset,
        len = ?req.len,
        response_body_bytes = tracing::field::Empty,
        error.kind = tracing::field::Empty,
        error.message = tracing::field::Empty,
        error.retryable = tracing::field::Empty,
    ))]
    pub fn read(&self, req: &wit_types::ReadBlobRequest) -> wit_types::CalloutResult {
        let result = match self.read_inner(req.blob, req.offset, req.len) {
            Ok(bytes) => wit_types::CalloutResult::BlobRead(bytes),
            Err(e) => e.into(),
        };
        record_outcome(&result);
        result
    }

    fn read_inner(
        &self,
        blob_id: u64,
        offset: u64,
        len: Option<u32>,
    ) -> Result<Vec<u8>, BlobError> {
        let record = self
            .cache
            .lookup_by_id(blob_id)
            .ok_or_else(|| BlobError::NotFound(format!("blob {blob_id} not found")))?;
        let path = self.cache.blob_path(&record.cache_key);
        read_range(&path, offset, len, self.limits.max_read_blob_bytes)
            .map_err(|error| error.with_io_context("read blob"))
    }
}

impl From<&BlobRecord> for wit_types::BlobFetched {
    fn from(record: &BlobRecord) -> Self {
        Self {
            blob: record.id,
            size: record.size,
            content_type: record.content_type.clone(),
            etag: record.etag.clone(),
            status: record.status,
            response_headers: record
                .response_headers
                .iter()
                .map(|(name, value)| wit_types::Header {
                    name: name.clone(),
                    value: value.clone(),
                })
                .collect(),
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

fn read_range(
    path: &Path,
    offset: u64,
    len: Option<u32>,
    max_bytes: u64,
) -> Result<Vec<u8>, BlobError> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthManager;
    use crate::capability::CapabilityChecker;
    use omnifs_caps::Allowlist;

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
        let tmp = tempfile::tempdir().unwrap();
        let cache = BlobCache::new(tmp.path().to_path_buf());
        assert_eq!(
            cache.metadata_path("pkg/foo.bin"),
            tmp.path().join(".meta/pkg/foo.bin.json")
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
        let record = cache.store(
            "blob".into(),
            BlobMetadata {
                status: 200,
                content_type: None,
                etag: None,
                response_headers: Vec::new(),
                size: 6,
            },
        );
        let capability = CapabilityChecker::new(Allowlist {
            domains: Vec::new(),
            git_repos: Vec::new(),
            needs_git: false,
            unix_sockets: Vec::new(),
        });
        let http =
            Arc::new(HttpStack::new(Arc::new(AuthManager::none()), Arc::new(capability)).unwrap());
        let executor = BlobExecutor::new(
            http,
            cache,
            BlobLimits {
                max_fetch_blob_bytes: DEFAULT_MAX_FETCH_BLOB_BYTES,
                max_read_blob_bytes: 4,
            },
        );

        match executor.read(&wit_types::ReadBlobRequest {
            blob: record.id,
            offset: 0,
            len: None,
        }) {
            wit_types::CalloutResult::CalloutError(wit_types::CalloutError {
                kind: wit_types::ErrorKind::TooLarge,
                retryable: false,
                ..
            }) => {},
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
        let metadata_path = cache_root
            .join(BLOB_META_DIR)
            .join(format!("{cache_key}.json"));
        std::fs::create_dir_all(blob_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(metadata_path.parent().unwrap()).unwrap();
        std::fs::write(&blob_path, b"hello world").unwrap();
        std::fs::write(
            &metadata_path,
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
    fn rehydrate_skips_blobs_without_valid_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_root = tmp.path().join("blob-cache");

        let malformed_key = "pkg-1.0/foo.bin";
        let malformed_blob = cache_root.join(malformed_key);
        let malformed_meta = cache_root
            .join(BLOB_META_DIR)
            .join(format!("{malformed_key}.json"));
        std::fs::create_dir_all(malformed_blob.parent().unwrap()).unwrap();
        std::fs::create_dir_all(malformed_meta.parent().unwrap()).unwrap();
        std::fs::write(&malformed_blob, b"hello world").unwrap();
        std::fs::write(&malformed_meta, b"{not json").unwrap();

        let missing_key = "old/path.bin";
        let missing_blob = cache_root.join(missing_key);
        std::fs::create_dir_all(missing_blob.parent().unwrap()).unwrap();
        std::fs::write(&missing_blob, b"stale").unwrap();

        let cache = BlobCache::new(cache_root);
        assert!(cache.lookup_by_key(malformed_key).is_none());
        assert!(cache.lookup_by_key(missing_key).is_none());
    }
}
