//! Typed async blob callout builders.
//!
//! `cx.http().get(url).into_blob().with_cache_key(key).send()` lands the
//! response body in the host's blob cache and returns a `BlobRef` —
//! metadata only. The bytes never cross the WIT boundary unless the
//! provider explicitly asks for them via `cx.blob(id).read().await`.
//!
//! A `BlobRef` is consumed by:
//! - `FileContent::blob(blob)` — serve the bytes verbatim from a `#[file]` handler.
//! - `cx.archives().open(blob).format(..).send()` — mount as a directory tree.
//! - `cx.blob(blob).read().await` — bring (a range of) the bytes across.

use crate::cx::Cx;
use crate::error::{ProviderError, Result};
use crate::http::CalloutFuture;
use crate::omnifs::provider::types::{
    BlobFetchRequest, BlobFetched, Callout, CalloutResult, Header, ReadBlobRequest,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BlobId(pub(crate) u64);

impl BlobId {
    pub fn raw(self) -> u64 {
        self.0
    }
}

impl From<u64> for BlobId {
    fn from(id: u64) -> Self {
        Self(id)
    }
}

#[derive(Clone, Debug)]
pub struct BlobRef {
    pub id: BlobId,
    pub size: u64,
    pub content_type: Option<String>,
    pub etag: Option<String>,
    pub status: u16,
    pub response_headers: Vec<(String, String)>,
}

impl BlobRef {
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
    match result {
        CalloutResult::BlobFetched(fetched) => Ok(BlobRef::from(fetched)),
        CalloutResult::CalloutError(e) => Err(ProviderError::from_callout_error(&e)),
        _ => Err(ProviderError::internal(
            "unexpected callout result for fetch-blob",
        )),
    }
}

/// Builder returned by `cx.blob(id)`.
pub struct BlobReader<'cx, S> {
    cx: &'cx Cx<S>,
    blob: BlobId,
}

impl<'cx, S> BlobReader<'cx, S> {
    pub(crate) fn new(cx: &'cx Cx<S>, blob: BlobId) -> Self {
        Self { cx, blob }
    }

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
    match result {
        CalloutResult::BlobRead(bytes) => Ok(bytes),
        CalloutResult::CalloutError(e) => Err(ProviderError::from_callout_error(&e)),
        _ => Err(ProviderError::internal(
            "unexpected callout result for read-blob",
        )),
    }
}
