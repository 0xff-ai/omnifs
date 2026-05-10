//! Provider-facing `fetch-blob` and `read-blob` executors.
//!
//! Provider HTTP fetches whose payload should never cross the WIT
//! boundary are streamed into [`crate::cache::blobs::BlobCache`].
//! Other host-side machinery (FUSE blob-backed reads, archive
//! extraction) consumes the file in place, so the bytes never
//! round-trip back through the provider.

use crate::auth::AuthManager;
#[cfg(test)]
use crate::cache::blobs::{BLOB_META_DIR, BlobMetadata};
use crate::cache::blobs::{
    BLOB_TMP_DIR, BlobCache, BlobCacheError, BlobRecordDraft, is_safe_path_segment,
};
use crate::runtime::capability::CapabilityChecker;
use crate::runtime::executor::{CalloutResponse, ErrorKind};
use crate::runtime::http_headers::{build_header_map, decode_response_headers};
use futures::StreamExt;
use std::io::Write;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

const FETCH_TIMEOUT: Duration = Duration::from_secs(120);
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

impl From<BlobCacheError> for BlobError {
    fn from(error: BlobCacheError) -> Self {
        match error {
            BlobCacheError::Io(error) => Self::Io(error),
        }
    }
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
            self.cache.cache_dir(),
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
        if let Err(error) = self.cache.store_metadata(&record) {
            return blob_error("store blob metadata", error.into());
        }
        if let Err(error) = staged.persist(&blob_path) {
            // Best-effort: stranded metadata is overwritten by the next fetch for this key.
            let _ = std::fs::remove_file(self.cache.metadata_path(cache_key));
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
        let tmp = tempfile::tempdir().unwrap();
        let cache = BlobCache::new(tmp.path().to_path_buf());
        assert_eq!(
            cache.metadata_path("pkg/foo.bin"),
            tmp.path().join(".meta/pkg/foo.bin.json")
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
    fn rehydrate_skips_blob_with_malformed_metadata() {
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
        std::fs::write(&metadata_path, b"{not json").unwrap();

        let cache = BlobCache::new(cache_root);
        assert!(cache.lookup_by_key(cache_key).is_none());
    }

    #[test]
    fn rehydrate_skips_blob_with_missing_metadata() {
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
