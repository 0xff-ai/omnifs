//! Typed async blob callout builders.
//!
//! Blobs keep large bodies host-side by design.
//! `cx.http().get(url).into_blob().with_cache_key(key).send()` (or the typed
//! [`crate::endpoint::RequestBuilder::into_blob`]) lands the response body in
//! the host's blob cache and returns a [`BlobRef`] carrying metadata only.
//! The cache key is provider-scoped: reusing a key from the same provider
//! deduplicates the fetch, and two providers never collide on a key.
//!
//! A stored blob is consumed by:
//! - [`crate::projection::FileProjection::blob`]: serve the bytes verbatim
//!   from a file route; the host reads them without guest involvement.
//! - `cx.archives().open(blob).format(..).send()`: mount as a directory tree
//!   (see [`crate::archives`]).

use crate::error::{ProviderError, Result};
use omnifs_wit::provider::types::{
    BlobFetchRequest, BlobFetched, Callout, CalloutResult, Header,
};

/// Runtime-local handle for a blob stored in the host cache. Valid only for
/// the current provider instance; do not persist it or derive paths from it.
/// The cache key, not this id, is the stable name for re-resolving a blob.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BlobId(pub(crate) u64);

impl BlobId {
    /// Return the raw WIT `blob-id` value.
    pub fn raw(self) -> u64 {
        self.0
    }
}

/// Metadata returned by a blob fetch.
///
/// The response body stays in the host cache. Use [`Self::id`] when passing
/// the blob to [`crate::projection::FileProjection::blob`], archive opening,
/// or [`BlobReader`]. The fetch resolves for any upstream status; check
/// [`Self::error_for_status`] before treating the blob as valid content.
#[derive(Clone, Debug)]
pub struct BlobRef {
    /// Runtime-local id of the cached blob.
    pub id: BlobId,
    /// Cached blob size in bytes.
    pub size: u64,
    /// Response `Content-Type`, when present.
    pub content_type: Option<String>,
    /// Response `ETag`, when present.
    pub etag: Option<String>,
    /// HTTP status returned by the upstream fetch.
    pub status: u16,
    /// Response headers preserved for provider inspection.
    pub response_headers: Vec<(String, String)>,
}

impl BlobRef {
    /// Default 4xx/5xx mapping. Mirrors the inline-HTTP `error_for_status`
    /// so blob fetches participate in the same error flow.
    pub fn error_for_status(self) -> Result<Self> {
        if (400..600).contains(&self.status) {
            Err(ProviderError::from_http_status(self.status))
        } else {
            Ok(self)
        }
    }
}

impl From<BlobFetched> for BlobRef {
    fn from(fetched: BlobFetched) -> Self {
        Self {
            id: BlobId(fetched.blob),
            size: fetched.size,
            content_type: fetched.content_type,
            etag: fetched.etag,
            status: fetched.status,
            response_headers: fetched
                .response_headers
                .into_iter()
                .map(|h| (h.name, h.value))
                .collect(),
        }
    }
}

pub(crate) fn blob_fetch_callout(
    method: String,
    url: String,
    headers: Vec<Header>,
    body: Option<Vec<u8>>,
    cache_key: String,
) -> Callout {
    Callout::FetchBlob(BlobFetchRequest {
        method,
        url,
        headers,
        body,
        cache_key,
    })
}

pub(crate) fn extract_blob(result: CalloutResult) -> Result<BlobRef> {
    crate::http::expect_callout(
        "fetch-blob",
        |r| match r {
            CalloutResult::BlobFetched(f) => Some(Ok(BlobRef::from(f))),
            _ => None,
        },
        result,
    )
}

