//! FUSE file open/read op boundary: delegate the read/open DECISION to
//! `Tree::read` / `Tree::open`, and keep the kernel-side handle tables (the
//! per-`fh` whole-file buffer, the ranged handle, inode size promotion) plus the
//! kernel offset/size slicing.

use super::Frontend;
use super::common::{FullReadTarget, RangedSlot};
use super::read_helpers::data_slice;
use fuser::{Errno, FileHandle as FuseFileHandle, FopenFlags, INodeNo, ReplyData};
use omnifs_api::events::TraceId;
use omnifs_core::view as view_types;
use omnifs_core::view::FileAttrsCache;
use omnifs_host::inspector::InspectorFuseScope;
use omnifs_tree::{Node, ReadResult, RequestCtx};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use tracing::warn;

impl Frontend {
    /// Serve a ranged read from a `Tree`-owned `RangedHandle` bound to this
    /// `fh`. `Tree` drives `read_chunk`, validates the chunk, and learns the
    /// exact size on an EOF-short read; the adapter promotes the learned size to
    /// the inode and replies with the chunk bytes.
    pub(super) async fn read_ranged_handle(
        &self,
        ino: u64,
        fh: u64,
        offset: u64,
        size: u32,
        reply: ReplyData,
    ) {
        let Some(slot) = self.ranged_handles.get(&fh) else {
            reply.error(Errno::EBADF);
            return;
        };
        let _permit = self.acquire_op_permit().await;
        match slot.handle.read(offset, size).await {
            Ok(chunk) => {
                if let Some(attrs) = chunk.learned_attrs {
                    if matches!(attrs.stability(), view_types::Stability::Live) {
                        // A live file's learned size grows monotonically (the
                        // ranged handle owns that decision); grow the inode so a
                        // polling `tail -f` re-stats and reads the appended
                        // bytes. `promote_inode_attrs` deliberately skips Live.
                        self.grow_live_size(ino, attrs.st_size());
                    } else {
                        self.promote_inode_attrs(ino, attrs);
                    }
                }
                reply.data(&chunk.bytes);
            },
            Err(error) => {
                warn!(error = %error, "ranged read_chunk error");
                reply.error(super::errno::tree_error_errno(&error));
            },
        }
    }

    /// Whole-file read for a provider or backing-fs file. A backing-fs file is
    /// read directly from the real filesystem (no provider round trip); a
    /// provider file is rendered through `Tree::read` (which owns the cache
    /// cascade, the write fence, and learned-size promotion). The rendered bytes
    /// populate the per-`fh` buffer so later reads of the same handle serve
    /// every offset from one buffer.
    pub(super) async fn read_full_handle(
        &self,
        ino: INodeNo,
        fh: FuseFileHandle,
        offset: u64,
        size: u32,
        live_scope: Option<InspectorFuseScope>,
        reply: ReplyData,
    ) {
        let Some(inode_entry) = self.inodes.get(&ino.0) else {
            if let Some(scope) = live_scope.as_ref() {
                scope.set_outcome(super::errno::inspector_outcome(Errno::ENOENT));
            }
            reply.error(Errno::ENOENT);
            return;
        };
        let mount_name = inode_entry.mount_name.clone();
        let path = inode_entry.path.clone();
        let backing_path = inode_entry.body.backing_path().cloned();
        let meta = inode_entry.meta();
        drop(inode_entry);

        let _permit = self.acquire_op_permit().await;

        // Drive the kernel-side invalidation fan-out before the read; `Tree::read`
        // owns the mem/durable cache cascade and the write fence.
        self.drain_and_evict_pending(&mount_name);

        // A backing-fs file is served from the real filesystem, never the
        // provider; `getattr` re-stats it, so no inode size promotion is needed.
        if let Some(ref rp) = backing_path {
            match std::fs::read(rp) {
                Ok(data) => {
                    reply.data(data_slice(&data, offset, size));
                    self.file_cache.insert(fh.0, data);
                },
                Err(e) => {
                    warn!(path = ?rp, err = %e, "backing fs error");
                    if let Some(scope) = live_scope.as_ref() {
                        scope.set_outcome(super::errno::inspector_outcome(Errno::EIO));
                    }
                    reply.error(Errno::EIO);
                },
            }
            return;
        }

        let node = Node::new(mount_name, path, meta, omnifs_tree::NodeBody::Provider);
        let ctx = RequestCtx {
            trace: live_scope.as_ref().map(InspectorFuseScope::trace_id),
        };
        match self.tree.read(&node, &ctx).await {
            Ok(ReadResult::Bytes { data, attrs, .. }) => {
                if let Some(attrs) = attrs {
                    self.promote_inode_attrs(ino.0, attrs);
                }
                reply.data(data_slice(&data, offset, size));
                self.file_cache.insert(fh.0, data);
            },
            Ok(ReadResult::Backing(dir)) => match std::fs::read(&dir) {
                Ok(data) => {
                    reply.data(data_slice(&data, offset, size));
                    self.file_cache.insert(fh.0, data);
                },
                Err(e) => {
                    warn!(path = ?dir, err = %e, "backing fs error");
                    if let Some(scope) = live_scope.as_ref() {
                        scope.set_outcome(super::errno::inspector_outcome(Errno::EIO));
                    }
                    reply.error(Errno::EIO);
                },
            },
            Err(error) => {
                if let Some(scope) = live_scope.as_ref() {
                    scope.set_outcome(super::errno::inspector_outcome(
                        super::errno::tree_error_errno(&error),
                    ));
                }
                reply.error(super::errno::tree_error_errno(&error));
            },
        }
    }

    /// The `open`-time dispatch. A host-synthesized control / ignore file is
    /// served once into the per-`fh` buffer (its bytes come from `Tree::read`,
    /// which runs the mutating pagination action exactly once); a ranged file
    /// opens a `Tree` `RangedHandle` bound to `fh`; an unknown-size full file is
    /// prefetched whole into the buffer. A backing-fs file and an exact-size full
    /// file open lazily (read on demand). Returns the kernel open flags, or an
    /// `Errno` for a resolution/render failure (e.g. an exhausted control).
    pub(super) async fn open_op(
        &self,
        target: &FullReadTarget,
        trace: Option<TraceId>,
    ) -> Result<FopenFlags, Errno> {
        let _permit = self.acquire_op_permit().await;
        self.drain_and_evict_pending(&target.mount_name);

        // Host-synthesized control / ignore files: re-resolve through `Tree`
        // (cache-only) to recover the synthetic byte source, then render once
        // into the per-`fh` buffer. A control whose feed has exhausted no longer
        // resolves (ENOENT); a real provider file of the same name is not
        // synthetic and falls through to the normal open path below.
        if target.is_synthetic_candidate()
            && let Some(flags) = self.open_synthetic_inner(target, trace).await?
        {
            return Ok(flags);
        }

        // Backing-fs files open lazily: `read` serves them from the real
        // filesystem.
        if target.body.is_backing() {
            return Ok(target.lazy_open_flags());
        }

        let node = target.provider_node();

        // A route declared `ranged` carries a `Deferred(Ranged)` placeholder, so
        // dispatch straight to `open_file` and bind a `RangedHandle` to `fh`. A
        // full file (the default) skips this and takes the full read path below,
        // so a whole-payload provider is asked exactly once. `Tree::open`
        // returning `None` means the route declared `ranged` but the handler
        // answered full (a provider bug): fall through and serve it as full.
        if target.is_ranged()
            && let Some(flags) = self.open_ranged_handle(target, &node, trace).await?
        {
            return Ok(flags);
        }

        // Unknown-size full-deferred files prefetch whole on open so `cat`/`ls`
        // see a learned size; an exact-size full file opens lazily.
        if target.should_prefetch_full() {
            let ctx = RequestCtx { trace };
            match self.tree.read(&node, &ctx).await {
                Ok(ReadResult::Bytes { data, attrs, .. }) => {
                    if let Some(attrs) = attrs {
                        self.promote_inode_attrs(target.ino, attrs);
                    }
                    self.file_cache.insert(target.fh, data);
                    return Ok(FopenFlags::FOPEN_DIRECT_IO);
                },
                Ok(ReadResult::Backing(_)) => {
                    // A full-deferred provider file never resolves to a backing
                    // dir; fall through to a lazy open.
                    return Ok(target.lazy_open_flags());
                },
                Err(error) => return Err(super::errno::tree_error_errno(&error)),
            }
        }

        Ok(target.lazy_open_flags())
    }

    /// Re-resolve a synthetic leaf through `Tree` and serve its bytes into the
    /// per-`fh` buffer. Returns `Ok(Some(flags))` when the leaf is synthetic,
    /// `Ok(None)` when `Tree` resolves a real provider file of the same name
    /// (caller continues the normal open path), `Err(ENOENT)` when an exhausted
    /// control no longer resolves.
    #[cfg(test)]
    pub(crate) async fn open_synthetic(
        &self,
        target: &FullReadTarget,
        trace: Option<TraceId>,
    ) -> Result<Option<FopenFlags>, Errno> {
        let _permit = self.acquire_op_permit().await;
        self.open_synthetic_inner(target, trace).await
    }

    async fn open_synthetic_inner(
        &self,
        target: &FullReadTarget,
        trace: Option<TraceId>,
    ) -> Result<Option<FopenFlags>, Errno> {
        let Some((parent_path, leaf)) = target.parent_and_leaf() else {
            return Ok(None);
        };
        let parent = Node::provider_dir(target.mount_name.clone(), parent_path);
        let ctx = RequestCtx { trace };
        // Resolve and render in one runtime entry: a `None` short-circuits when
        // the leaf is a real provider file (the caller falls through), so the
        // read only runs for a genuine synthetic node.
        let outcome = async {
            let node = self.tree.resolve_child(&parent, &leaf, &ctx).await?;
            if !node.is_synthetic() {
                return Ok(None);
            }
            self.tree.read(&node, &ctx).await.map(Some)
        }
        .await;
        match outcome {
            // A real provider file (e.g. a provider-projected `.gitignore`) wins;
            // the caller serves it through the normal read path.
            Ok(None) => Ok(None),
            Ok(Some(ReadResult::Bytes { data, attrs, .. })) => {
                if let Some(attrs) = attrs {
                    self.promote_inode_attrs(target.ino, attrs);
                }
                self.file_cache.insert(target.fh, data);
                Ok(Some(FopenFlags::FOPEN_DIRECT_IO))
            },
            Ok(Some(ReadResult::Backing(_))) => Err(Errno::EIO),
            Err(error) => Err(super::errno::tree_error_errno(&error)),
        }
    }

    /// Probe `Tree::open` for a deferred file and bind the `RangedHandle` to
    /// `fh` when the source is ranged, spawning a follow pump for a live file.
    /// `Ok(Some(flags))` => ranged, opened; `Ok(None)` => not a ranged source,
    /// the caller falls through to the full read path.
    async fn open_ranged_handle(
        &self,
        target: &FullReadTarget,
        node: &Node,
        trace: Option<TraceId>,
    ) -> Result<Option<FopenFlags>, Errno> {
        let ctx = RequestCtx { trace };
        let Some(handle) = self
            .tree
            .open(node, &ctx)
            .await
            .map_err(|e| super::errno::tree_error_errno(&e))?
        else {
            return Ok(None);
        };
        let attrs = handle.attrs().clone();
        self.promote_inode_attrs(target.ino, attrs.clone());
        // A live file (tail -f) grows while observed. Spawn a follow pump that
        // learns upstream growth on a cadence and records it in `follow_sizes`,
        // which `getattr` reports so an idle reader sees the file grow. The
        // size-learning is `Tree`'s; the reporting is ours.
        if matches!(attrs.stability(), view_types::Stability::Live) {
            self.spawn_follow_pump(
                target.ino,
                target.fh,
                target.mount_name.clone(),
                handle.provider_handle(),
                handle.observed_end(),
            );
        }
        self.ranged_handles.insert(
            target.fh,
            RangedSlot {
                ino: target.ino,
                handle,
            },
        );
        Ok(Some(FopenFlags::FOPEN_DIRECT_IO))
    }

    pub(super) fn promote_inode_attrs(&self, ino: u64, attrs: FileAttrsCache) {
        if matches!(attrs.stability(), view_types::Stability::Live) {
            return;
        }
        let Some(mut entry) = self.inodes.get_mut(&ino) else {
            return;
        };
        entry.size = attrs.st_size();
        entry.attrs = Some(attrs);
    }

    /// Grow a live file's cached inode size from an observed end, never
    /// shrinking. The file stays live, so it keeps direct I/O and a zero attr
    /// TTL; a polling `tail -f` re-stats, sees the new size, and reads forward.
    /// Rotation/truncation is handled by the reader reopening, not by a size
    /// that moves backwards mid-follow.
    fn grow_live_size(&self, ino: u64, observed_end: u64) {
        let Some(mut entry) = self.inodes.get_mut(&ino) else {
            return;
        };
        if observed_end <= entry.size {
            return;
        }
        entry.size = observed_end;
        if let Some(attrs) = entry.attrs.take() {
            entry.attrs = Some(attrs.with_exact_size(observed_end));
        }
    }

    /// Spawn a background pump for a live file: on a cadence it asks `Tree` to
    /// probe upstream growth (a sizing read at the current end), recording any
    /// new end in `follow_sizes`. `getattr` reports that size, so a polling
    /// `tail -f` re-stats (TTL=0), sees growth, and reads the new bytes through
    /// the normal ranged path. Aborted on `release`.
    fn spawn_follow_pump(
        &self,
        ino: u64,
        fh: u64,
        mount_name: String,
        provider_handle: u64,
        observed_end: Arc<AtomicU64>,
    ) {
        let follow_sizes = self.follow_sizes.clone();
        let pump = omnifs_tree::spawn_live_follow_pump(
            &self.rt,
            self.registry.clone(),
            mount_name,
            provider_handle,
            observed_end,
            move |new_end| {
                follow_sizes
                    .entry(ino)
                    .and_modify(|current| *current = (*current).max(new_end))
                    .or_insert(new_end);
            },
        );
        self.follow_pumps.insert(fh, pump);
    }
}
