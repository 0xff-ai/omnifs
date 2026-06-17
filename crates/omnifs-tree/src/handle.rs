//! Ranged read handle and the `Tree::open` body.
//!
//! `Tree::open` dispatches `Namespace::open_file` for a `Deferred(Ranged)`
//! file and returns a runtime-owned `RangedHandle`. `RangedHandle::read` drives
//! `Namespace::read_chunk` and learns the exact size on an EOF-short read. Fully
//! async (no `block_on`): the renderer binds the handle to its own kernel handle
//! (FUSE fh / NFS stateid) and drives reads from its own executor.

use std::sync::Arc;

use omnifs_core::path::Path;
use omnifs_core::view as view_types;
use omnifs_core::view::FileAttrsCache;
use omnifs_host::wit_protocol::{file_size_from_wit, stability_from_wit};
use omnifs_host::{Error, Runtime};

use crate::error::{Result, TreeError};
use crate::node::Node;
use crate::read::{Chunk, learned_ranged_eof_attrs};
use crate::{RequestCtx, Tree};

/// Runtime-owned ranged read handle for `Deferred(Ranged)` files. Holds an
/// `Arc<Runtime>` so it is self-contained across renderer calls. The renderer
/// binds it to its own kernel handle (FUSE fh / NFS stateid) and owns ONLY that
/// mapping + lease; the provider handle (u64) and chunk validation are neutral.
pub struct RangedHandle {
    pub(crate) runtime: Arc<Runtime>,
    pub(crate) path: Path,
    pub(crate) provider_handle: u64,
    pub(crate) attrs: FileAttrsCache,
}

impl RangedHandle {
    pub fn attrs(&self) -> &FileAttrsCache {
        &self.attrs
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn provider_handle(&self) -> u64 {
        self.provider_handle
    }

    /// Drive provider `read_chunk` for one ranged read; learn the exact size on
    /// an EOF-short read. Validates the chunk against the requested length and
    /// the projected attrs, mirroring the FUSE ranged read path.
    pub async fn read(&self, offset: u64, length: u32) -> Result<Chunk> {
        let chunk = match self
            .runtime
            .namespace()
            .read_chunk(self.provider_handle, offset, length)
            .await
        {
            Ok(chunk) => chunk,
            Err(Error::ProviderError(error)) => {
                return Err(Error::ProviderError(error).into());
            },
            Err(error) => return Err(error.into()),
        };

        if chunk.content.len() > length as usize {
            return Err(TreeError::internal(format!(
                "provider returned oversized ranged chunk for {}: requested {length}, returned {}",
                self.path.as_str(),
                chunk.content.len()
            )));
        }

        let mut learned_attrs = None;
        if chunk.eof {
            let content_len = u64::try_from(chunk.content.len()).map_err(|_| {
                TreeError::internal(format!(
                    "ranged chunk length does not fit u64 for {}",
                    self.path.as_str()
                ))
            })?;
            let eof_size = offset.checked_add(content_len).ok_or_else(|| {
                TreeError::internal(format!(
                    "ranged EOF offset overflow for {}",
                    self.path.as_str()
                ))
            })?;
            if let Err(error) = self.attrs.validate_observed_size(eof_size) {
                return Err(TreeError::internal(format!(
                    "provider returned ranged EOF that contradicts file attrs for {}: {error}",
                    self.path.as_str()
                )));
            }
            learned_attrs = learned_ranged_eof_attrs(self.attrs.clone(), eof_size);
        }

        Ok(Chunk {
            bytes: chunk.content,
            eof: chunk.eof,
            learned_attrs,
        })
    }

    /// Release the provider handle via `Runtime::call_close_file`. Sync: the
    /// pass-through is sync. The renderer calls this when its fh/stateid is
    /// released or invalidated.
    pub fn close(self) -> Result<()> {
        self.runtime
            .call_close_file(self.provider_handle)
            .map_err(Into::into)
    }
}

impl Tree {
    /// Open a `Deferred(Ranged)` file: dispatch `Namespace::open_file`, derive
    /// the opened attrs from the projected attrs + the provider's reported size,
    /// and return a runtime-owned `RangedHandle` the renderer binds to its
    /// kernel handle. Faithful port of the FUSE `open_ranged_file` path.
    pub async fn open(&self, node: &Node, _ctx: &RequestCtx) -> Result<RangedHandle> {
        let projected = node.attrs().ok_or_else(|| {
            TreeError::invalid_input(format!(
                "open requires a ranged file projection: {}",
                node.path().as_str()
            ))
        })?;
        if !matches!(
            projected.bytes,
            view_types::ByteSource::Deferred(view_types::ReadMode::Ranged)
        ) {
            return Err(TreeError::invalid_input(format!(
                "open requires byte-source::deferred(read-mode::ranged): {}",
                node.path().as_str()
            )));
        }

        let runtime = self.runtime_for(node.mount())?;
        let opened = runtime.namespace().open_file(node.path().as_str()).await?;

        let attrs = opened_file_attrs(projected, &opened.attrs);
        Ok(RangedHandle {
            runtime,
            path: node.path().clone(),
            provider_handle: opened.handle,
            attrs,
        })
    }
}

/// Derive the opened ranged handle's attrs: the provider's reported size and
/// stability over the projection's `Deferred(Ranged)` byte source. The caller
/// has already verified the projection is `Deferred(Ranged)`.
fn opened_file_attrs(
    projected: &FileAttrsCache,
    opened: &omnifs_wit::provider::types::FileAttrs,
) -> FileAttrsCache {
    FileAttrsCache {
        size: file_size_from_wit(opened.size),
        bytes: projected.bytes.clone(),
        stability: stability_from_wit(opened.stability),
        version_token: opened.version_token.clone(),
    }
}
