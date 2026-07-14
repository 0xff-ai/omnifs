//! Typed async archive-mount callout builders.
//!
//! `cx.archives().open(blob).format(ArchiveFormat::TarGz).strip_prefix("foo/").send()`
//! asks the host to extract a stored blob to disk and returns a [`TreeRef`].
//! A handler registered via `r.treeref(t).handler(h)` returns that `TreeRef`
//! and the host resolves it the same way as a `git-open-repo` tree, serving
//! the extracted directory through FUSE bind mounts. Neither the archive
//! bytes nor the extracted entries ever enter guest memory; the provider
//! only brokers the handle.
//!
//! The blob must already be in the host blob cache (see [`crate::blob`]);
//! fetch it first with `.into_blob()`. Typical shape:
//!
//! ```ignore
//! let blob = cx
//!     .endpoint::<ArxivWeb>()
//!     .get(source_path)
//!     .into_blob()
//!     .fetch()
//!     .await?;
//! let tree = cx.archives().open(blob.id).send().await?;
//! ```

use crate::blob::BlobId;
use crate::cx::Cx;
use crate::handler::TreeRef;
use crate::http::CalloutFuture;
use omnifs_wit::provider::types::{
    ArchiveFormat as WitArchiveFormat, ArchiveOpenRequest, Callout, CalloutResult,
};

/// Archive formats accepted by `open-archive`. A closed set on purpose:
/// the host owns extraction, so new formats are added to the WIT enum and
/// here together rather than sniffed from bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArchiveFormat {
    /// Gzip-compressed tar archive.
    TarGz,
    /// Uncompressed tar archive.
    Tar,
    /// Zip archive.
    Zip,
}

impl From<ArchiveFormat> for WitArchiveFormat {
    fn from(format: ArchiveFormat) -> Self {
        match format {
            ArchiveFormat::TarGz => Self::TarGz,
            ArchiveFormat::Tar => Self::Tar,
            ArchiveFormat::Zip => Self::Zip,
        }
    }
}

/// Entry point for archive callout builders.
pub struct Builder<'cx, S> {
    cx: &'cx Cx<S>,
}

impl<'cx, S> Builder<'cx, S> {
    pub fn new(cx: &'cx Cx<S>) -> Self {
        Self { cx }
    }

    /// Begin an `open-archive` callout for a cached blob.
    pub fn open(self, blob: BlobId) -> OpenRequest<'cx, S> {
        OpenRequest::new(self.cx, blob)
    }
}

/// Builder for an `open-archive` callout.
#[must_use]
pub struct OpenRequest<'cx, S> {
    cx: &'cx Cx<S>,
    blob: BlobId,
    format: ArchiveFormat,
    strip_prefix: Option<String>,
}

impl<'cx, S> OpenRequest<'cx, S> {
    fn new(cx: &'cx Cx<S>, blob: BlobId) -> Self {
        Self {
            cx,
            blob,
            format: ArchiveFormat::TarGz,
            strip_prefix: None,
        }
    }

    /// Override the archive format. Default is [`ArchiveFormat::TarGz`].
    pub fn format(mut self, format: ArchiveFormat) -> Self {
        self.format = format;
        self
    }

    /// Strip a leading directory from each archive entry's path before
    /// it lands on disk. Common for `cargo publish`-style tarballs whose
    /// top-level wrapper is `<name>-<version>/`.
    pub fn strip_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.strip_prefix = Some(prefix.into());
        self
    }

    /// Lower into an `open-archive` callout. Awaiting suspends the
    /// operation; the host extracts the blob to disk and the resumed future
    /// yields the [`TreeRef`] to return from a treeref route. Re-derive the
    /// handle per operation rather than persisting it: the host caches
    /// extraction by blob, format, and strip prefix, so repeated opens are
    /// cheap.
    pub fn send(self) -> CalloutFuture<'cx, TreeRef> {
        let request = ArchiveOpenRequest {
            blob: self.blob.raw(),
            format: self.format.into(),
            strip_prefix: self.strip_prefix,
        };
        CalloutFuture::new(self.cx, Callout::OpenArchive(request), |r| {
            crate::http::expect_callout(
                "open-archive",
                |r| match r {
                    CalloutResult::ArchiveOpened(opened) => Some(Ok(TreeRef::new(opened.tree))),
                    _ => None,
                },
                r,
            )
        })
    }
}
