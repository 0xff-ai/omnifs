//! Provider-facing `fetch-blob` executor.
//!
//! Provider HTTP fetches whose payload should never cross the WIT
//! boundary are streamed into the mount-owned blob cache.
//! Other host-side machinery consumes the file in place, so the bytes never
//! round-trip back through the provider.

use crate::cache::MountResources;
use crate::cache::blob::{BLOB_TMP_DIR, BlobCacheError, BlobMetadata, BlobRecord};
use crate::cache::identity::{BlobGeneration, BlobRequestId};
use crate::callouts::{callout_internal, callout_network, callout_too_large, record_outcome};
use crate::http::{HttpStack, decode_response_headers};
use crate::log_redaction::{LogUrl, WitHeaders};
use futures::StreamExt;
use omnifs_wit::provider::types as wit_types;
use omnifs_workspace::mounts::Spec;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

const BLOB_FETCH_TIMEOUT: Duration = Duration::from_mins(2);

const DEFAULT_MAX_FETCH_BLOB_BYTES: u64 = 1024 * 1024 * 1024;
/// Host-side size limits for blob fetches.
///
/// Fetch limits bound how much data the host stores on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlobLimits {
    /// Maximum response-body bytes accepted by `fetch-blob`.
    pub max_fetch_blob_bytes: u64,
}

impl Default for BlobLimits {
    fn default() -> Self {
        Self {
            max_fetch_blob_bytes: DEFAULT_MAX_FETCH_BLOB_BYTES,
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
        }
    }
}

/// Errors raised while fetching or storing host-resident blobs.
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
    #[error("I/O error")]
    Io(#[source] std::io::Error),
    #[error("{0}")]
    Internal(String),
}

impl From<std::io::Error> for BlobError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
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
            BlobCacheError::Io(_) => Self::Internal("blob cache I/O failed".to_string()),
            BlobCacheError::Internal(message) => Self::Internal(message),
        }
    }
}

impl From<BlobError> for wit_types::CalloutResult {
    fn from(error: BlobError) -> Self {
        match error {
            BlobError::TooLarge { .. } => callout_too_large(error.to_string()),
            BlobError::Network(_) => callout_network(error.to_string()),
            BlobError::Io(_) => callout_internal(error.to_string()),
            BlobError::Internal(msg) => callout_internal(msg),
        }
    }
}

/// Executes provider blob callouts against a host-owned disk cache.
#[derive(Clone)]
pub struct BlobExecutor {
    http: Arc<HttpStack>,
    resources: Arc<MountResources>,
    limits: BlobLimits,
}

impl BlobExecutor {
    /// Construct an executor with explicit host blob limits.
    pub fn new(http: Arc<HttpStack>, resources: Arc<MountResources>, limits: BlobLimits) -> Self {
        Self {
            http,
            resources,
            limits,
        }
    }

    /// Fetch an HTTP response body into the blob cache.
    ///
    /// The body is streamed to a temporary file and published by the cache
    /// only after the response has stayed within the configured fetch cap.
    #[tracing::instrument(target = "omnifs_callout", skip_all, fields(
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
        let result =
            match self
                .http
                .validate(&req.method, &req.url, &req.headers, req.body.as_deref())
            {
                Err(early) => early,
                Ok(request) => {
                    let request_id = request
                        .blob_request_id(self.http.auth_binding_for_url(request.original_url()));
                    let lock = self.resources.blob.request_lock(request_id);
                    let _guard = lock.lock().await;
                    if let Some(record) = self.resources.blob.lookup_by_request(request_id) {
                        wit_types::CalloutResult::BlobFetched(record.as_ref().into())
                    } else {
                        match self.http.send_validated(&request, BLOB_FETCH_TIMEOUT).await {
                            Ok(response) => match self.materialize(request_id, response).await {
                                Ok(record) => {
                                    wit_types::CalloutResult::BlobFetched((&record).into())
                                },
                                Err(error) => error.into(),
                            },
                            Err(early) => early,
                        }
                    }
                },
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
        request_id: BlobRequestId,
        response: reqwest::Response,
    ) -> Result<BlobRecord, BlobError> {
        let status = response.status().as_u16();
        let response_headers = decode_response_headers(response.headers());
        let etag = lookup_header(&response_headers, "etag");
        let content_type = lookup_header(&response_headers, "content-type");

        let staged = stream_response_body(
            response,
            self.resources.blob.cache_dir(),
            self.limits.max_fetch_blob_bytes,
        )
        .await
        .map_err(|error| error.with_io_context("fetch blob"))?;
        let size = staged.size;
        let generation = staged.generation();

        let metadata = BlobMetadata {
            status,
            content_type,
            etag,
            response_headers,
            size,
        };
        let record = self
            .resources
            .blob
            .publish(request_id, generation, staged.path(), metadata)
            .map_err(|error| BlobError::from(error).with_io_context("publish blob"))?;
        Ok((*record).clone())
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
    hasher: blake3::Hasher,
}

impl StagedBlob {
    fn generation(&self) -> BlobGeneration {
        BlobGeneration::from_hash(self.hasher.clone().finalize())
    }

    fn path(&self) -> &Path {
        self.tmp.path()
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
    let root_metadata = std::fs::symlink_metadata(cache_root)?;
    if !root_metadata.is_dir() || root_metadata.file_type().is_symlink() {
        return Err(BlobError::Internal(
            "blob cache root is not an owned directory".to_string(),
        ));
    }
    match std::fs::symlink_metadata(&cache_dir) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {},
        Ok(_) => {
            return Err(BlobError::Internal(
                "blob temp directory is not an owned directory".to_string(),
            ));
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            tokio::fs::create_dir(&cache_dir).await?;
        },
        Err(error) => return Err(error.into()),
    }
    let mut tmp = tempfile::Builder::new()
        .prefix("fetch-")
        .suffix(".tmp")
        .tempfile_in(cache_dir)
        .map_err(BlobError::Io)?;
    let mut stream = response.bytes_stream();
    let mut total = 0_u64;
    let mut hasher = blake3::Hasher::new();
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
        hasher.update(&chunk);
    }
    tmp.flush().map_err(BlobError::Io)?;
    Ok(StagedBlob {
        tmp,
        size: total,
        hasher,
    })
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
    use crate::cache::blob::BlobCache;
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
        std::fs::rename(staged.path(), &path).expect("publish staged body");
        assert_eq!(std::fs::read(&path).unwrap(), b"abcde");
    }

    #[test]
    fn publishes_body_before_ref_and_rehydrates_only_coherent_refs() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_root = tmp.path().join("blob-cache");
        let cache = BlobCache::new(cache_root.clone()).unwrap();
        let request = BlobRequestId::new(None, "GET", "https://example.test/blob", &[], None);
        let body = b"hello world";
        let generation = BlobGeneration::from_bytes(body);
        let staged = cache_root.join(BLOB_TMP_DIR).join("staged");
        std::fs::write(&staged, body).unwrap();
        let _record = cache
            .publish(
                request,
                generation,
                &staged,
                BlobMetadata {
                    status: 200,
                    content_type: Some("text/plain".into()),
                    etag: Some("etag-1".into()),
                    response_headers: vec![("x-test".into(), "value".into())],
                    size: body.len() as u64,
                },
            )
            .unwrap();
        assert!(!staged.exists());
        assert!(
            cache_root
                .join("objects")
                .join(generation.filesystem_name())
                .is_file()
        );

        let cache = BlobCache::new(cache_root.clone()).unwrap();
        let record = cache
            .lookup_by_request(request)
            .expect("rehydrated request ref");
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
    fn rehydrate_skips_a_ref_with_wrong_size_or_digest() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_root = tmp.path().join("blob-cache");
        let _cache = BlobCache::new(cache_root.clone()).unwrap();
        let request = BlobRequestId::new(None, "GET", "https://example.test/bad", &[], None);
        let generation = BlobGeneration::from_bytes(b"actual");
        let body = cache_root
            .join("objects")
            .join(generation.filesystem_name());
        std::fs::create_dir_all(body.parent().unwrap()).unwrap();
        std::fs::write(&body, b"actual").unwrap();
        let reference = cache_root
            .join("refs")
            .join(format!("{}.json", request.filesystem_name()));
        std::fs::write(
            reference,
            serde_json::to_vec(&serde_json::json!({
                "request_id": request.filesystem_name(),
                "generation": generation.filesystem_name(),
                "status": 200,
                "content_type": null,
                "etag": null,
                "response_headers": [],
                "size": 99,
            }))
            .unwrap(),
        )
        .unwrap();
        let cache = BlobCache::new(cache_root).unwrap();
        assert!(cache.lookup_by_request(request).is_none());
    }
}
