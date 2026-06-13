//! File open/read paths for FUSE (full, ranged, synthetic).

use super::Frontend;
use super::common::{FullReadTarget, RangedFileHandle, split_parent_leaf};
use super::read_helpers::{
    data_slice, file_payload_for_attrs, full_read_matches_attrs, learned_full_read_attrs,
    learned_ranged_eof_attrs, opened_file_attrs, resolve_read_payload,
    should_prefetch_full_on_open,
};
use fuser::{Errno, FileHandle as FuseFileHandle, FopenFlags, INodeNo, ReplyData};
use omnifs_cache::{Record as CacheRecord, RecordKind};
use omnifs_core::path::Path;
use omnifs_core::view as view_types;
use omnifs_core::view::{FileAttrsCache, FilePayload};
use omnifs_host::inspector::InspectorFuseScope;
use omnifs_host::pagination;
use omnifs_host::{Error, Runtime};
use omnifs_inspector::{CacheKind, InspectorOutcome, TraceId};
use omnifs_wit::provider::types::{ByteSource, ReadFileResult};
use std::time::Instant;
use tracing::{debug, warn};

impl Frontend {
    pub(super) fn read_ranged_handle(
        &self,
        ino: u64,
        ranged: &RangedFileHandle,
        offset: u64,
        size: u32,
        reply: ReplyData,
    ) {
        let Some(runtime) = self.runtime_for_mount(&ranged.mount_name) else {
            reply.error(Errno::ENOENT);
            return;
        };

        match self.rt.block_on(
            runtime
                .namespace()
                .read_chunk(ranged.provider_handle, offset, size),
        ) {
            Ok(chunk) => {
                if chunk.content.len() > size as usize {
                    warn!(
                        path = ranged.path.as_str(),
                        requested = size,
                        returned = chunk.content.len(),
                        "provider returned oversized ranged chunk"
                    );
                    reply.error(Errno::EIO);
                    return;
                }
                if chunk.eof {
                    let Some(content_len) = u64::try_from(chunk.content.len()).ok() else {
                        reply.error(Errno::EIO);
                        return;
                    };
                    let Some(eof_size) = offset.checked_add(content_len) else {
                        reply.error(Errno::EIO);
                        return;
                    };
                    if matches!(ranged.attrs.stability, view_types::Stability::Volatile) {
                        // A volatile file (tail -f shapes) is meant to change
                        // while observed, so a freshly observed end never
                        // contradicts the open-time size. Grow the inode size
                        // monotonically so getattr reports the new length and a
                        // polling reader re-stats and reads the appended bytes.
                        self.grow_volatile_size(ino, eof_size);
                    } else {
                        if let Err(error) = ranged.attrs.validate_observed_size(eof_size) {
                            warn!(
                                path = ranged.path.as_str(),
                                error, "provider returned ranged EOF that contradicts file attrs"
                            );
                            reply.error(Errno::EIO);
                            return;
                        }
                        if let Some(attrs) =
                            learned_ranged_eof_attrs(ranged.attrs.clone(), eof_size)
                        {
                            self.promote_inode_attrs(ino, attrs);
                        }
                    }
                }
                reply.data(&chunk.content);
            },
            Err(Error::ProviderError(error)) => {
                warn!(
                    path = ranged.path.as_str(),
                    kind = ?error.kind,
                    retryable = error.retryable,
                    message = error.message,
                    "provider returned typed error for read_chunk"
                );
                reply.error(super::errno::provider_error_errno(&error));
            },
            Err(error) => {
                warn!(path = ranged.path.as_str(), error = %error, "read_chunk runtime error");
                reply.error(Errno::EIO);
            },
        }
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn read_full_handle(
        &self,
        ino: INodeNo,
        fh: FuseFileHandle,
        offset: u64,
        size: u32,
        live: Option<&InspectorFuseScope>,
        reply: ReplyData,
    ) {
        let Some(inode_entry) = self.inodes.get(&ino.0) else {
            if let Some(scope) = live {
                scope.set_outcome(super::errno::inspector_outcome(Errno::ENOENT));
            }
            reply.error(Errno::ENOENT);
            return;
        };
        let target = FullReadTarget {
            ino: ino.0,
            fh: fh.0,
            mount_name: inode_entry.mount_name.clone(),
            path: inode_entry.path.clone(),
            backing_path: inode_entry.backing_path.clone(),
            attrs: inode_entry.attrs.clone(),
            synthetic: inode_entry.synthetic,
        };
        drop(inode_entry);

        self.drain_and_evict_pending(&target.mount_name);
        let cache_started = Instant::now();
        let elapsed = || cache_started.elapsed();

        if let Some(attrs) = target.attrs.as_ref()
            && matches!(attrs.size, view_types::FileSize::Exact(0))
        {
            reply.data(&[]);
            return;
        }

        let durable_aux = target
            .attrs
            .as_ref()
            .and_then(FileAttrsCache::durable_cache_aux);

        if let Some(aux) = durable_aux.clone()
            && let Some(record) = self.mem_get_with_aux(
                &target.mount_name,
                &target.path,
                RecordKind::File,
                aux.as_deref(),
            )
            && let Some(payload) = file_payload_for_attrs(&record, target.attrs.as_ref())
        {
            debug!(target: "omnifs_cache", kind = "mem_hit", op = "read", mount = target.mount_name.as_str(), "cache hit");
            if let Some(scope) = live {
                scope.emit_cache(CacheKind::FileHit, elapsed());
            }
            reply.data(data_slice(&payload.content, offset, size));
            self.file_cache.insert(target.fh, payload.content);
            return;
        }

        if target.backing_path.is_none()
            && let Some(aux) = durable_aux.clone()
            && let Some(runtime) = self.runtime_for_mount(&target.mount_name)
            && let Some(record) = runtime.cache_get(&target.path, RecordKind::File, aux.as_deref())
            && let Some(payload) = file_payload_for_attrs(&record, target.attrs.as_ref())
        {
            debug!(target: "omnifs_cache", kind = "disk_hit", op = "read", mount = target.mount_name.as_str(), "cache hit");
            if let Some(scope) = live {
                scope.emit_cache(CacheKind::FileHit, elapsed());
            }
            let data = payload.content;
            reply.data(data_slice(&data, offset, size));
            self.file_cache.insert(target.fh, data);
            return;
        }

        if let Some(ref rp) = target.backing_path {
            match std::fs::read(rp) {
                Ok(data) => {
                    reply.data(data_slice(&data, offset, size));
                    self.file_cache.insert(target.fh, data);
                },
                Err(e) => {
                    warn!(path = ?rp, err = %e, "backing fs error");
                    if let Some(scope) = live {
                        scope.set_outcome(super::errno::inspector_outcome(Errno::EIO));
                    }
                    reply.error(Errno::EIO);
                },
            }
            return;
        }

        let Some(runtime) = self.runtime_for_mount(&target.mount_name) else {
            if let Some(scope) = live {
                scope.set_outcome(super::errno::inspector_outcome(Errno::ENOENT));
            }
            reply.error(Errno::ENOENT);
            return;
        };

        self.drain_and_evict_pending(&target.mount_name);

        debug!(target: "omnifs_cache", kind = "miss", op = "read", mount = target.mount_name.as_str(), "cache miss");

        // Derive the content type the host echoes into `read-file`: the
        // fixed suffix map for the four standard representation extensions,
        // else the SDK-supplied content type (none retrievable on a cold
        // miss; see TODO below).
        // TODO: a bare-name field/custom-suffix leaf has its
        // SDK-supplied content type only on the cached FilePayload, which is
        // absent on a cold read. Until the canonical/render path is wired to
        // providers, such leaves cold-read as application/octet-stream. The
        // standard `.md/.json/.xml/.raw` representations are unaffected.
        let path = Path::parse(&target.path).expect("inode path must be a protocol path");
        let content_type = path.content_type_mime(None).to_string();
        // Capture the generation before the read so the rendered result can be
        // fenced against an invalidation that lands mid-read (Codex #1).
        let op_gen = runtime.current_generation();
        match self.rt.block_on(runtime.namespace().read_file(
            &target.path,
            content_type,
            live.map(InspectorFuseScope::trace_id),
        )) {
            Ok(result) => {
                self.finish_full_read(&target, &runtime, offset, size, result, op_gen, reply);
            },
            Err(Error::ProviderError(error)) => {
                warn!(
                    path = target.path.as_str(),
                    kind = ?error.kind,
                    retryable = error.retryable,
                    message = error.message,
                    "provider returned typed error for read_file"
                );
                if let Some(scope) = live {
                    scope.set_outcome(super::errno::inspector_outcome(
                        super::errno::provider_error_errno(&error),
                    ));
                }
                reply.error(super::errno::provider_error_errno(&error));
            },
            Err(error) => {
                warn!(path = target.path.as_str(), error = %error, "read_file runtime error");
                if let Some(scope) = live {
                    scope.set_outcome(InspectorOutcome::Internal);
                }
                reply.error(Errno::EIO);
            },
        }
    }

    // All arguments are load-bearing on this private read-completion helper; the
    // `op_gen` fence parameter (Codex #1) pushed it one over the lint threshold.
    #[allow(clippy::too_many_arguments)]
    fn finish_full_read(
        &self,
        target: &FullReadTarget,
        runtime: &Runtime,
        offset: u64,
        size: u32,
        result: ReadFileResult,
        op_gen: u64,
        reply: ReplyData,
    ) {
        // An identity representation answered by reference to the canonical
        // store (`byte-source::canonical`) is not copied into the View cache:
        // the canonical store is its sole home, so caching it here would
        // duplicate the bytes across both stores (ADR-0001 §4, hybrid policy).
        let from_canonical = matches!(result.bytes, ByteSource::Canonical);
        let Some((data, result_attrs, content_type)) =
            resolve_read_payload(runtime, &target.path, result)
        else {
            reply.error(Errno::EIO);
            return;
        };
        debug!(
            target: "omnifs_read",
            path = target.path.as_str(),
            content_len = data.len(),
            "received Read result"
        );
        let attrs_cache = learned_full_read_attrs(result_attrs, data.len());
        if !full_read_matches_attrs(&attrs_cache, data.len()) {
            warn!(
                path = target.path.as_str(),
                expected = ?attrs_cache.size,
                actual = data.len(),
                "provider returned bytes that contradict file attrs"
            );
            reply.error(Errno::EIO);
            return;
        }
        self.promote_inode_attrs(target.ino, attrs_cache.clone());
        if !from_canonical
            && self
                .cache_durable_file_payload(
                    &target.mount_name,
                    &target.path,
                    &attrs_cache,
                    &data,
                    content_type,
                    op_gen,
                )
                .is_err()
        {
            reply.error(Errno::EIO);
            return;
        }
        reply.data(data_slice(&data, offset, size));
        self.file_cache.insert(target.fh, data);
    }

    fn cache_durable_file_payload(
        &self,
        mount_name: &str,
        path: &str,
        attrs_cache: &FileAttrsCache,
        data: &[u8],
        content_type: Option<String>,
        op_gen: u64,
    ) -> Result<(), Errno> {
        let Some(aux) = attrs_cache.durable_cache_aux() else {
            return Ok(());
        };
        let payload = FilePayload::new(attrs_cache.version_token.clone(), data.to_vec())
            .with_content_type(content_type);
        let Some(payload) = payload.serialize() else {
            return Err(Errno::EIO);
        };
        let file_record = CacheRecord::new(RecordKind::File, payload);
        if let Some(rt) = self.runtime_for_mount(mount_name) {
            // Drop the write if an invalidation for this path landed after the
            // read began: caching it would reinstate stale bytes (Codex #1).
            if rt.write_fenced(path, op_gen) {
                return Ok(());
            }
            rt.cache_put(path, RecordKind::File, aux.as_deref(), &file_record);
        }
        Ok(())
    }

    pub(super) fn open_ranged_file(
        &self,
        target: &FullReadTarget,
    ) -> Result<Option<FopenFlags>, Errno> {
        // Synthetic/inline/blob files resolve before here and backing-tree
        // files are served from disk. A deferred file may be ranged or full,
        // and a cheap lookup leaves only a `Deferred(Full)` placeholder on the
        // inode, so probe `open_file`: success means the source is ranged; a
        // non-ranged source reports `InvalidInput` and falls through to the
        // full read path below.
        if target.backing_path.is_some()
            || !target
                .attrs
                .as_ref()
                .is_some_and(|attrs| matches!(attrs.bytes, view_types::ByteSource::Deferred(_)))
        {
            return Ok(None);
        }

        let Some(runtime) = self.runtime_for_mount(&target.mount_name) else {
            return Err(Errno::ENOENT);
        };
        match self
            .rt
            .block_on(runtime.namespace().open_file(&target.path))
        {
            Ok(opened) => {
                let opened_attrs = opened_file_attrs(&opened.attrs);
                self.promote_inode_attrs(target.ino, opened_attrs.clone());
                let is_volatile = matches!(opened_attrs.stability, view_types::Stability::Volatile);
                self.ranged_handles.insert(
                    target.fh,
                    RangedFileHandle {
                        mount_name: target.mount_name.clone(),
                        path: target.path.clone(),
                        provider_handle: opened.handle,
                        attrs: opened_attrs,
                    },
                );
                if is_volatile {
                    self.spawn_follow_pump(
                        target.ino,
                        target.fh,
                        target.mount_name.clone(),
                        opened.handle,
                    );
                }
                Ok(Some(FopenFlags::FOPEN_DIRECT_IO))
            },
            Err(Error::ProviderError(error))
                if error.kind == omnifs_wit::provider::types::ErrorKind::InvalidInput =>
            {
                // The file's source is not ranged; fall through to the full
                // read path.
                Ok(None)
            },
            Err(Error::ProviderError(error)) => Err(super::errno::provider_error_errno(&error)),
            Err(error) => {
                warn!(
                    path = target.path.as_str(),
                    error = %error,
                    "open_file runtime error"
                );
                Err(Errno::EIO)
            },
        }
    }

    pub(super) fn prefetch_full_file_on_open(
        &self,
        target: &FullReadTarget,
        fuse_trace: Option<TraceId>,
    ) -> Result<Option<FopenFlags>, Errno> {
        if target.backing_path.is_some()
            || !target
                .attrs
                .as_ref()
                .is_some_and(should_prefetch_full_on_open)
        {
            return Ok(None);
        }

        let Some(runtime) = self.runtime_for_mount(&target.mount_name) else {
            return Err(Errno::ENOENT);
        };
        self.drain_and_evict_pending(&target.mount_name);
        // Same content-type derivation as the read path; see the TODO
        // there about bare-name leaves on a cold read.
        let path = Path::parse(&target.path).expect("inode path must be a protocol path");
        let content_type = path.content_type_mime(None).to_string();
        let op_gen = runtime.current_generation();
        match self.rt.block_on(runtime.namespace().read_file(
            &target.path,
            content_type,
            fuse_trace,
        )) {
            Ok(result) => {
                // See the hybrid policy in `finish_full_read`: a canonical
                // reference is served from the canonical store, not duplicated
                // into the View cache.
                let from_canonical = matches!(result.bytes, ByteSource::Canonical);
                let Some((data, result_attrs, content_type)) =
                    resolve_read_payload(&runtime, &target.path, result)
                else {
                    return Err(Errno::EIO);
                };
                let attrs_cache = learned_full_read_attrs(result_attrs, data.len());
                if !full_read_matches_attrs(&attrs_cache, data.len()) {
                    warn!(
                        path = target.path.as_str(),
                        expected = ?attrs_cache.size,
                        actual = data.len(),
                        "provider returned bytes that contradict file attrs"
                    );
                    return Err(Errno::EIO);
                }
                self.promote_inode_attrs(target.ino, attrs_cache.clone());
                if !from_canonical {
                    self.cache_durable_file_payload(
                        &target.mount_name,
                        &target.path,
                        &attrs_cache,
                        &data,
                        content_type,
                        op_gen,
                    )?;
                }
                self.file_cache.insert(target.fh, data);
                Ok(Some(FopenFlags::FOPEN_DIRECT_IO))
            },
            Err(Error::ProviderError(error)) => Err(super::errno::provider_error_errno(&error)),
            Err(error) => {
                warn!(
                    path = target.path.as_str(),
                    error = %error,
                    "read_file runtime error during open"
                );
                Err(Errno::EIO)
            },
        }
    }

    /// Serve a host-synthetic control (`@next`/`@all`) or mount-root ignore
    /// file at `open` time, materializing its content into the per-`fh` file
    /// cache so `read` serves every offset from the same buffer.
    ///
    /// These files are never a provider `read_file`; running the (mutating)
    /// control action exactly once per open avoids the prefetch path
    /// [`prefetch_full_file_on_open`](Self::prefetch_full_file_on_open) issuing
    /// a `read_file("@next")` the provider cannot answer, and avoids a partial
    /// or repeated `read` advancing pagination multiple times.
    ///
    /// Returns `Ok(Some(flags))` when the inode is synthetic and now served
    /// from `fh`, `Ok(None)` when it is an ordinary file (caller continues to
    /// the normal open path). Ignore files are served here only when their
    /// inode was marked `synthetic` at lookup, so a real provider `.gitignore`
    /// is never shadowed.
    pub(super) fn open_synthetic_file(
        &self,
        target: &FullReadTarget,
        fuse_trace: Option<TraceId>,
    ) -> Result<Option<FopenFlags>, Errno> {
        let Some((parent_path, leaf)) = split_parent_leaf(&target.path) else {
            return Ok(None);
        };

        // `@next`/`@all`: only synthetic while the parent's cached dirents still
        // carry the control (i.e. a resume cursor remains). A control whose
        // entry is gone is no longer a file.
        if pagination::is_control_name(&leaf) {
            if self
                .cached_control_dirent(&target.mount_name, &parent_path, &leaf)
                .is_none()
            {
                return Err(Errno::ENOENT);
            }
            let Some(status) =
                self.serve_control_read(&target.mount_name, &parent_path, &leaf, fuse_trace)
            else {
                return Err(Errno::ENOENT);
            };
            let bytes = status.into_bytes();
            // Promote the inode size to the freshly generated status length so
            // `cat` reads the whole message instead of the `Unknown`
            // placeholder's single byte. Mirrors `prefetch_full_file_on_open`.
            self.promote_inode_attrs(
                target.ino,
                pagination::control_read_attrs(bytes.len() as u64),
            );
            self.file_cache.insert(target.fh, bytes);
            return Ok(Some(FopenFlags::FOPEN_DIRECT_IO));
        }

        // Mount-root ignore files: serve from `fh` ONLY when the inode was
        // marked synthetic at lookup time (i.e. the provider returned no such
        // file and the host synthesized it). A real provider `.gitignore` keeps
        // `synthetic == false` and is served normally through the read path,
        // never shadowed by content keyed on the path/name.
        if target.synthetic
            && super::common::is_mount_root(&parent_path)
            && pagination::is_ignore_name(&leaf)
        {
            self.file_cache
                .insert(target.fh, pagination::IGNORE_CONTENT.as_bytes().to_vec());
            return Ok(Some(FopenFlags::FOPEN_DIRECT_IO));
        }

        Ok(None)
    }

    fn promote_inode_attrs(&self, ino: u64, attrs: FileAttrsCache) {
        if matches!(attrs.stability, view_types::Stability::Volatile) {
            return;
        }
        let Some(mut entry) = self.inodes.get_mut(&ino) else {
            return;
        };
        entry.size = attrs.st_size();
        entry.attrs = Some(attrs);
    }

    /// Grow a volatile file's cached size from an observed ranged EOF, never
    /// shrinking. The file stays volatile, so it keeps direct I/O and a zero
    /// attr TTL (`ttl_for_attrs` only grants the long TTL to immutable exact
    /// files), which is what lets a polling `tail -f` see the new size on its
    /// next `stat` and read forward. Rotation/truncation is handled by the
    /// reader reopening, not by a size that moves backwards mid-follow.
    fn grow_volatile_size(&self, ino: u64, observed_end: u64) {
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

    /// Spawn a background pump for a volatile file: on a cadence it probes the
    /// provider's `read_chunk` at the current end purely to learn the upstream
    /// size, recording it in `follow_sizes`. `getattr` reports that size, so a
    /// polling `tail -f` re-stats (TTL=0), sees growth, and reads the new bytes
    /// through the normal ranged path. The pump is the only caller that reads
    /// just to size; reads and probes serialize on the provider's store lock,
    /// and an offset-addressable reader serves both consistently. Aborted on
    /// `release`.
    fn spawn_follow_pump(&self, ino: u64, fh: u64, mount_name: String, provider_handle: u64) {
        const PROBE_LEN: u32 = 64 * 1024;
        const INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);
        let registry = self.registry.clone();
        let follow_sizes = self.follow_sizes.clone();
        let task = self.rt.spawn(async move {
            let mut known_end: u64 = 0;
            loop {
                tokio::time::sleep(INTERVAL).await;
                let Some(runtime) = registry.get(&mount_name) else {
                    break;
                };
                match runtime
                    .namespace()
                    .read_chunk(provider_handle, known_end, PROBE_LEN)
                    .await
                {
                    Ok(chunk) => {
                        let advanced = u64::try_from(chunk.content.len()).unwrap_or(0);
                        if advanced > 0 {
                            known_end = known_end.saturating_add(advanced);
                            follow_sizes.insert(ino, known_end);
                        }
                    },
                    Err(_) => break,
                }
            }
        });
        self.follow_pumps.insert(fh, task.abort_handle());
    }
}
