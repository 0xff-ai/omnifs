//! Typed async blob callout builders.
//!
//! `cx.http().get(url).into_blob().with_cache_key(key).send()` lands the
//! response body in the host's blob cache and returns a `BlobRef` with
//! metadata only. The bytes never cross the WIT boundary unless the
//! provider explicitly asks for them via `cx.blob(id).read().await`;
//! each read response is capped by host policy because those bytes
//! cross into guest memory.
//!
//! A `BlobRef` is consumed by:
//! - `FileContent::blob(blob)`: serve the bytes verbatim from a `#[file]` handler.
//! - `cx.archives().open(blob).format(..).send()`: mount as a directory tree.
//! - `cx.blob(blob).read().await`: bring a range of bytes across.

use crate::cx::Cx;
use crate::error::{ProviderError, Result};
use crate::http::CalloutFuture;
use omnifs_wit::provider::types::{
    BlobFetchRequest, BlobFetched, Callout, CalloutResult, Header, ReadBlobRequest,
};

/// Runtime-local handle for a blob stored in the host cache.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BlobId(pub(crate) u64);

impl BlobId {
    /// Return the raw WIT `blob-id` value.
    pub fn raw(self) -> u64 {
        self.0
    }
}

impl From<u64> for BlobId {
    fn from(id: u64) -> Self {
        Self(id)
    }
}

/// Metadata returned by a blob fetch.
///
/// The response body stays in the host cache. Use [`Self::id`] when
/// passing the blob to `FileContent::blob`, archive opening, or
/// [`BlobReader`].
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
    /// Runtime-local id of the cached blob.
    pub fn id(&self) -> BlobId {
        self.id
    }

    /// Default 4xx/5xx mapping. Mirrors the inline-HTTP `error_for_status`
    /// so blob fetches participate in the same error flow.
    pub fn error_for_status(self) -> Result<Self> {
        if (400..600).contains(&self.status) {
            Err(ProviderError::from_http_status(self.status))
        } else {
            Ok(self)
        }
    }

    /// Return a response header value by case-insensitive name.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.response_headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
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

/// Builder returned by `cx.blob(id)`.
///
/// Reads copy cached bytes back into guest memory, so each response is
/// capped by host policy. Serve large files with `FileContent::blob`
/// or expose archives as `TreeRef`s when the provider does not need
/// the bytes in memory.
pub struct BlobReader<'cx, S> {
    cx: &'cx Cx<S>,
    blob: BlobId,
}

impl<'cx, S> BlobReader<'cx, S> {
    pub(crate) fn new(cx: &'cx Cx<S>, blob: BlobId) -> Self {
        Self { cx, blob }
    }

    /// Read the whole blob from offset zero to EOF.
    ///
    /// This is only suitable for blobs small enough to fit under the
    /// host's `read-blob` cap.
    pub fn read(self) -> CalloutFuture<'cx, S, Vec<u8>> {
        CalloutFuture::new(
            self.cx,
            Callout::ReadBlob(ReadBlobRequest {
                blob: self.blob.0,
                offset: 0,
                len: None,
            }),
            extract_blob_read,
        )
    }

    /// Read at most `len` bytes starting at `offset`.
    ///
    /// Offsets past EOF return an empty byte vector. The host cap
    /// applies to the returned byte count, not to the absolute offset.
    pub fn read_range(self, offset: u64, len: u32) -> CalloutFuture<'cx, S, Vec<u8>> {
        CalloutFuture::new(
            self.cx,
            Callout::ReadBlob(ReadBlobRequest {
                blob: self.blob.0,
                offset,
                len: Some(len),
            }),
            extract_blob_read,
        )
    }
}

fn extract_blob_read(result: CalloutResult) -> Result<Vec<u8>> {
    crate::http::expect_callout(
        "read-blob",
        |r| match r {
            CalloutResult::BlobRead(bytes) => Some(Ok(bytes)),
            _ => None,
        },
        result,
    )
}
