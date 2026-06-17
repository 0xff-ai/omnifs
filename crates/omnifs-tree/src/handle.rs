//! Ranged read handle and the `Tree::open` probe.
//!
//! `Tree::open` probes `Namespace::open_file` for a deferred file and returns a
//! runtime-owned `RangedHandle` when the source is ranged (`None` when it is
//! not, so the renderer falls through to a full read). `RangedHandle::read`
//! drives `Namespace::read_chunk` and learns the exact size on an EOF-short
//! read, growing a live file monotonically. Fully async (no `block_on`): the
//! renderer binds the handle to its own kernel handle (FUSE fh / NFS stateid)
//! and drives reads from its own executor.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

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
    /// Monotonic high-water mark of the observed upstream end, shared with the
    /// renderer's live-follow loop. A `Live` read grows it on EOF; the loop
    /// probes from it and advances it. The renderer reports this size through
    /// its own getattr so a polling `tail -f` sees the file grow.
    pub(crate) observed_end: Arc<AtomicU64>,
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

    /// Shared monotonic high-water mark of the observed upstream end. The
    /// renderer clones this to drive a live-follow loop (see
    /// [`probe_live_growth`]) and to report the live size from its getattr.
    pub fn observed_end(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.observed_end)
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
            if matches!(self.attrs.stability, view_types::Stability::Live) {
                // A live file (tail -f shapes) is meant to change while observed,
                // so a freshly observed end never contradicts the open-time size.
                // Grow the shared high-water mark monotonically and learn that
                // size, so the renderer reports a growing file to a polling
                // reader. No validation: there is no fixed size to contradict.
                let end = self
                    .observed_end
                    .fetch_max(eof_size, Ordering::Relaxed)
                    .max(eof_size);
                learned_attrs = Some(self.attrs.clone().with_exact_size(end));
            } else {
                if let Err(error) = self.attrs.validate_observed_size(eof_size) {
                    return Err(TreeError::internal(format!(
                        "provider returned ranged EOF that contradicts file attrs for {}: {error}",
                        self.path.as_str()
                    )));
                }
                learned_attrs = learned_ranged_eof_attrs(self.attrs.clone(), eof_size);
            }
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
    /// Probe `Namespace::open_file` for a deferred file and, when the provider's
    /// source is ranged, return a runtime-owned `RangedHandle` the renderer binds
    /// to its kernel handle. A cheap lookup leaves only a `Deferred(Full)`
    /// placeholder on the node, so the real read mode is discovered here: a
    /// non-ranged source reports `InvalidInput`, and a path with no file route
    /// (an object representation or projected leaf) reports `NotFound`. Either
    /// way this returns `Ok(None)` so the renderer falls through to the full read
    /// path. Faithful port of the FUSE `open_ranged_file` probe.
    pub async fn open(&self, node: &Node, _ctx: &RequestCtx) -> Result<Option<RangedHandle>> {
        let projected = node.attrs().ok_or_else(|| {
            TreeError::invalid_input(format!(
                "open requires a deferred file projection: {}",
                node.path().as_str()
            ))
        })?;
        if !matches!(projected.bytes, view_types::ByteSource::Deferred(_)) {
            return Err(TreeError::invalid_input(format!(
                "open requires byte-source::deferred: {}",
                node.path().as_str()
            )));
        }

        let runtime = self.runtime_for(node.mount())?;
        let opened = match runtime.namespace().open_file(node.path().as_str()).await {
            Ok(opened) => opened,
            Err(Error::ProviderError(error))
                if matches!(
                    error.kind,
                    omnifs_wit::provider::types::ErrorKind::InvalidInput
                        | omnifs_wit::provider::types::ErrorKind::NotFound
                ) =>
            {
                return Ok(None);
            },
            Err(error) => return Err(error.into()),
        };

        Ok(Some(RangedHandle {
            runtime,
            path: node.path().clone(),
            provider_handle: opened.handle,
            attrs: opened_file_attrs(&opened.attrs),
            observed_end: Arc::new(AtomicU64::new(0)),
        }))
    }
}

/// Probe a live file's upstream end without serving bytes to a reader: read
/// `probe_len` at the current `observed_end` purely to learn whether the file
/// grew, advancing `observed_end` monotonically. Returns the new end when it
/// grew, `None` when it did not. The renderer's follow loop calls this on its
/// own cadence; both frontends share this learning so a live file's size source
/// is neutral and the reporting stays frontend-specific.
pub async fn probe_live_growth(
    runtime: &Runtime,
    provider_handle: u64,
    observed_end: &AtomicU64,
    probe_len: u32,
) -> Result<Option<u64>> {
    let known_end = observed_end.load(Ordering::Relaxed);
    let chunk = runtime
        .namespace()
        .read_chunk(provider_handle, known_end, probe_len)
        .await?;
    let advanced = u64::try_from(chunk.content.len()).unwrap_or(0);
    if advanced == 0 {
        return Ok(None);
    }
    let new_end = known_end.saturating_add(advanced);
    observed_end.fetch_max(new_end, Ordering::Relaxed);
    Ok(Some(new_end))
}

/// Derive the opened ranged handle's attrs from the provider's `open_file`
/// result. A successful `open_file` means the source is ranged regardless of the
/// cheap `Deferred(Full)` placeholder a lookup left on the node, so the byte
/// source is fixed to `Deferred(Ranged)`; the real size and stability come from
/// the open result.
fn opened_file_attrs(opened: &omnifs_wit::provider::types::FileAttrs) -> FileAttrsCache {
    FileAttrsCache {
        size: file_size_from_wit(opened.size),
        bytes: view_types::ByteSource::Deferred(view_types::ReadMode::Ranged),
        stability: stability_from_wit(opened.stability),
        version_token: opened.version_token.clone(),
    }
}
