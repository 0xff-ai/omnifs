//! Ranged read handle and the provider open probe.
//!
//! The internal open path probes the provider for a deferred file and returns a
//! runtime-owned `RangedHandle` when the source is ranged (`None` when it is
//! not, so the renderer falls through to a full read). `RangedHandle::read`
//! drives `Namespace::read_chunk` and learns the exact size on an EOF-short
//! read, growing a live file monotonically. Fully async (no `block_on`): the
//! renderer binds the handle to its own kernel handle (FUSE fh / NFS stateid)
//! and drives reads from its own executor.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::registry::MountRuntimes;
use crate::view as view_types;
use crate::view::FileAttrsCache;
use crate::{EngineError, Runtime};
use omnifs_core::path::Path;
use tokio::runtime::Handle;

use super::error::{Result, TreeError};
use super::node::Node;
use super::read::{Chunk, FileAttrStore, enforce_declared_materialize_cap};
use crate::TreeNamespace;

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
    pub(crate) fn attrs(&self) -> &FileAttrsCache {
        &self.attrs
    }

    pub(crate) fn provider_handle(&self) -> u64 {
        self.provider_handle
    }

    /// Shared monotonic high-water mark of the observed upstream end. The
    /// renderer clones this to drive a live-follow loop (see
    /// [`probe_live_growth`]) and to report the live size from its getattr.
    pub(crate) fn observed_end(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.observed_end)
    }

    /// Drive provider `read_chunk` for one ranged read; learn the exact size on
    /// an EOF-short read. The typed Runtime boundary validates the requested
    /// length before this handle-level EOF and projected-attr bookkeeping.
    pub(crate) async fn read(&self, offset: u64, length: u32) -> Result<Chunk> {
        let chunk = match self
            .runtime
            .read_chunk(self.provider_handle, offset, length)
            .await
        {
            Ok(chunk) => chunk,
            Err(EngineError::ProviderError(error)) => {
                return Err(EngineError::ProviderError(error).into());
            },
            Err(error) => return Err(error.into()),
        };

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
            if matches!(self.attrs.stability(), view_types::Stability::Live) {
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
                learned_attrs = self.attrs.learned_ranged_eof_attrs(eof_size).map_err(|error| {
                    TreeError::internal(format!(
                        "provider returned ranged EOF that contradicts file attrs for {}: {error}",
                        self.path.as_str()
                    ))
                })?;
                if let Some(attrs) = &learned_attrs {
                    FileAttrStore::new(&self.runtime, &self.path).publish(attrs.clone())?;
                }
            }
        }

        Ok(Chunk {
            bytes: chunk.content,
            eof: chunk.eof,
            learned_attrs,
        })
    }

    /// Release the provider handle without consuming the handle. Used by the
    /// namespace handle cache, which owns the sole reference to a cached handle
    /// and closes it exactly once at eviction, where a consuming `close` cannot
    /// be called through a shared owner.
    pub(crate) fn release(&self) -> Result<()> {
        self.runtime
            .call_close_file(self.provider_handle)
            .map_err(Into::into)
    }
}

impl TreeNamespace {
    /// Probe `Namespace::open_file` for a deferred file and, when the provider's
    /// source is ranged, return a runtime-owned `RangedHandle` the renderer binds
    /// to its kernel handle. A cheap lookup leaves only a `Deferred(Full)`
    /// placeholder on the node, so the real read mode is discovered here: a
    /// non-ranged source reports `InvalidInput`, and a path with no file route
    /// (an object representation or projected leaf) reports `NotFound`. Either
    /// way this returns `Ok(None)` so the renderer falls through to the full read
    /// path.
    pub(crate) async fn open(&self, node: &Node) -> Result<Option<RangedHandle>> {
        let projected = node.attrs().ok_or_else(|| {
            TreeError::invalid_input(format!(
                "open requires a deferred file projection: {}",
                node.path().as_str()
            ))
        })?;
        if !projected.is_deferred() {
            return Err(TreeError::invalid_input(format!(
                "open requires byte-source::deferred: {}",
                node.path().as_str()
            )));
        }
        if projected.is_deferred_full() {
            enforce_declared_materialize_cap(node.path(), Some(projected))?;
            return Ok(None);
        }

        let runtime = self.runtime_for(node.mount())?;
        let opened = match runtime.open_file(node.path()).await {
            Ok(opened) => opened,
            Err(error) if error.is_provider_not_found_or_invalid_input() => return Ok(None),
            Err(error) => return Err(error.into()),
        };

        let attrs = opened.attrs;
        FileAttrStore::new(&runtime, node.path()).publish(attrs.clone())?;
        Ok(Some(RangedHandle {
            runtime,
            path: node.path().clone(),
            provider_handle: opened.handle,
            attrs,
            observed_end: Arc::new(AtomicU64::new(0)),
        }))
    }

    /// Probe a deferred ranged file's real attrs by opening it through the
    /// provider, then immediately closing the provider handle. This is for
    /// frontends such as NFS that must render child attrs during directory
    /// flattening. The probe is intentionally named as provider I/O, and the
    /// learned attrs are published through the shared view cache before returning.
    pub(crate) async fn probe_ranged_attrs(
        &self,
        mount: &str,
        path: &Path,
    ) -> Result<Option<FileAttrsCache>> {
        let runtime = self.runtime_for(mount)?;
        let opened = match runtime.open_file(path).await {
            Ok(opened) => opened,
            Err(error) if error.is_provider_not_found_or_invalid_input() => return Ok(None),
            Err(error) => return Err(error.into()),
        };

        let attrs = opened.attrs;
        let validation = attrs.validate().map_err(|error| {
            TreeError::internal(format!(
                "open-file returned invalid attrs for {path}: {error}"
            ))
        });
        let close = runtime.call_close_file(opened.handle);
        if let Err(error) = close {
            tracing::warn!(path = %path, error = %error, "ranged attr probe close failed");
        }
        validation?;
        FileAttrStore::new(&runtime, path).publish(attrs.clone())?;
        Ok(Some(attrs))
    }
}

/// Probe a live file's upstream end without serving bytes to a reader: read
/// `probe_len` at the current `observed_end` purely to learn whether the file
/// grew, advancing `observed_end` monotonically. Returns the new end when it
/// grew, `None` when it did not. The renderer's follow loop calls this on its
/// own cadence; both frontends share this learning so a live file's size source
/// is neutral and the reporting stays frontend-specific.
pub(crate) async fn probe_live_growth(
    runtime: &Runtime,
    provider_handle: u64,
    observed_end: &AtomicU64,
    probe_len: u32,
) -> Result<Option<u64>> {
    let known_end = observed_end.load(Ordering::Relaxed);
    let chunk = runtime
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

/// Spawn the shared live-file growth probe loop. Renderers own the reported
/// size table, so `record_growth` is frontend-specific.
pub(crate) fn spawn_live_follow_pump(
    rt: &Handle,
    registry: Arc<MountRuntimes>,
    mount_name: String,
    provider_handle: u64,
    observed_end: Arc<AtomicU64>,
    mut record_growth: impl FnMut(u64) + Send + 'static,
) -> tokio::task::AbortHandle {
    const PROBE_LEN: u32 = 64 * 1024;
    const INTERVAL: Duration = Duration::from_secs(1);
    let task = rt.spawn(async move {
        loop {
            tokio::time::sleep(INTERVAL).await;
            let Some(runtime) = registry.get(&mount_name) else {
                break;
            };
            match probe_live_growth(&runtime, provider_handle, &observed_end, PROBE_LEN).await {
                Ok(Some(new_end)) => record_growth(new_end),
                Ok(None) => {},
                Err(_) => break,
            }
        }
    });
    task.abort_handle()
}
