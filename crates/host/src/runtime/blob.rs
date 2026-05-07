//! Provider-facing blob cache and `fetch-blob` / `read-blob` executors.
//!
//! Provider HTTP fetches whose payload should never cross the WIT
//! boundary land here. The body is streamed to a disk file under
//! `<cache_dir>/blobs/<provider-scope>/<cache-key>` and a `blob-id`
//! handle is returned. Other host-side machinery (FUSE blob-backed
//! reads, archive extraction) consumes the file in place — the bytes
//! never round-trip back through the provider.

use crate::auth::AuthManager;
use crate::runtime::capability::CapabilityChecker;
use crate::runtime::executor::{CalloutResponse, ErrorKind};
use dashmap::DashMap;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;
use tracing::warn;

const FETCH_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone)]
pub struct BlobRecord {
    pub id: u64,
    pub path: PathBuf,
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
/// IDs and the in-memory key index are NOT persisted across host
/// restarts in v1 — the on-disk file persists, but a restart re-fetches
/// on first reuse. Persisting the index is a future enhancement.
pub struct BlobCache {
    cache_dir: PathBuf,
    keys: DashMap<String, u64>,
    blobs: DashMap<u64, Arc<BlobRecord>>,
    locks: DashMap<String, Arc<AsyncMutex<()>>>,
    next_id: AtomicU64,
}

impl BlobCache {
    pub fn new(cache_dir: PathBuf) -> Self {
        Self {
            cache_dir,
            keys: DashMap::new(),
            blobs: DashMap::new(),
            locks: DashMap::new(),
            next_id: AtomicU64::new(1),
        }
    }

    pub fn lookup(&self, blob_id: u64) -> Option<Arc<BlobRecord>> {
        self.blobs.get(&blob_id).map(|r| r.clone())
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// Register a pre-existing record under its declared id and key.
    /// Used by tests; production code should always go through
    /// [`BlobExecutor::fetch_blob`].
    #[cfg(test)]
    pub(crate) fn insert_for_test(&self, key: &str, record: BlobRecord) {
        let id = record.id;
        self.blobs.insert(id, Arc::new(record));
        self.keys.insert(key.to_string(), id);
    }

    fn key_lock(&self, key: &str) -> Arc<AsyncMutex<()>> {
        self.locks
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BlobError {
    #[error("unsafe cache key: {0}")]
    UnsafeCacheKey(String),
    #[error("unsafe provider scope: {0}")]
    UnsafeProviderScope(String),
    #[error("blob {0} not found")]
    NotFound(u64),
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
        if component.contains(std::path::MAIN_SEPARATOR) && std::path::MAIN_SEPARATOR != '/' {
            return false;
        }
    }
    true
}

pub struct BlobExecutor {
    client: reqwest::Client,
    auth: Arc<AuthManager>,
    capability: Arc<CapabilityChecker>,
    cache: Arc<BlobCache>,
}

impl BlobExecutor {
    pub fn new(
        auth: Arc<AuthManager>,
        capability: Arc<CapabilityChecker>,
        cache: Arc<BlobCache>,
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
        })
    }

    pub fn cache(&self) -> &Arc<BlobCache> {
        &self.cache
    }

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
        if let Some(id) = self.cache.keys.get(cache_key).map(|r| *r)
            && let Some(record) = self.cache.blobs.get(&id).map(|r| r.clone())
        {
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
        let response_headers = read_response_headers(response.headers());
        let etag = lookup_header(&response_headers, "etag");
        let content_type = lookup_header(&response_headers, "content-type");

        let bytes = match response.bytes().await {
            Ok(b) => b,
            Err(e) => return error(ErrorKind::Network, e.to_string(), true),
        };

        // Persist to disk.
        let blob_path = self.cache.cache_dir.join(cache_key);
        if let Some(parent) = blob_path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            return error(ErrorKind::Internal, format!("create blob dir: {e}"), false);
        }
        if let Err(e) = atomic_write(&blob_path, &bytes) {
            return error(ErrorKind::Internal, format!("write blob: {e}"), false);
        }

        let id = self.cache.next_id.fetch_add(1, Ordering::Relaxed);
        let record = Arc::new(BlobRecord {
            id,
            path: blob_path,
            size: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            content_type,
            etag,
            status,
            response_headers,
        });
        self.cache.blobs.insert(id, record.clone());
        self.cache.keys.insert(cache_key.to_string(), id);

        CalloutResponse::BlobFetched((*record).clone())
    }

    pub fn read_blob(&self, blob_id: u64, offset: u64, len: Option<u32>) -> CalloutResponse {
        let Some(record) = self.cache.lookup(blob_id) else {
            return error(
                ErrorKind::NotFound,
                format!("blob {blob_id} not found"),
                false,
            );
        };
        match read_range(&record.path, offset, len) {
            Ok(bytes) => CalloutResponse::BlobRead(bytes),
            Err(e) => error(ErrorKind::Internal, format!("read blob: {e}"), false),
        }
    }
}

fn read_range(path: &Path, offset: u64, len: Option<u32>) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut buf = Vec::new();
    match len {
        Some(n) => {
            let mut limited = (&mut file).take(u64::from(n));
            limited.read_to_end(&mut buf)?;
        },
        None => {
            file.read_to_end(&mut buf)?;
        },
    }
    Ok(buf)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn build_header_map(
    auth: &[(String, String)],
    request: &[(String, String)],
) -> Result<HeaderMap, String> {
    let mut map = HeaderMap::new();
    append_headers(&mut map, auth, "auth")?;
    append_headers(&mut map, request, "request")?;
    Ok(map)
}

fn append_headers(
    map: &mut HeaderMap,
    headers: &[(String, String)],
    source: &str,
) -> Result<(), String> {
    for (name, value) in headers {
        let header_name = HeaderName::from_str(name)
            .map_err(|e| format!("invalid {source} header name `{name}`: {e}"))?;
        let header_value = HeaderValue::from_str(value).map_err(|e| {
            format!(
                "invalid {source} header value for `{}`: {e}",
                header_name.as_str()
            )
        })?;
        map.append(header_name, header_value);
    }
    Ok(())
}

fn read_response_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter_map(|(name, value)| match value.to_str() {
            Ok(value) => Some((name.as_str().to_string(), value.to_string())),
            Err(error) => {
                warn!(header = %name, err = %error, "dropping non-UTF8 response header");
                None
            },
        })
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_path_segment_rejects_traversal() {
        assert!(is_safe_path_segment("crates/serde-1.0.197.crate"));
        assert!(!is_safe_path_segment("../etc/passwd"));
        assert!(!is_safe_path_segment("/abs"));
        assert!(!is_safe_path_segment(""));
        assert!(!is_safe_path_segment("a//b"));
        assert!(!is_safe_path_segment("a/./b"));
    }

    #[test]
    fn lookup_header_is_case_insensitive() {
        let headers = vec![("Content-Type".into(), "text/plain".into())];
        assert_eq!(
            lookup_header(&headers, "content-type"),
            Some("text/plain".into())
        );
    }
}
