//! Directory listing snapshots and `opendir` for FUSE.

use super::Frontend;
use super::common::{
    DirSnapshot, file_kind_placeholder, is_mount_root, join_child_path, root_ignore_meta,
};
use super::errno::inspector_outcome;
use fuser::{Errno, Generation, ReplyEntry};
use omnifs_cache::{Record as CacheRecord, RecordKind};
use omnifs_core::view::{DirentRecord, DirentsPayload};
use omnifs_host::inspector::InspectorFuseScope;
use omnifs_host::wit_protocol;
use omnifs_host::{Error, Runtime, pagination};
use omnifs_inspector::{CacheKind, TraceId};
use omnifs_wit::provider::types::{self as wit_types, ErrorKind, ListChildrenResult};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, warn};

impl Frontend {
    /// Build a directory snapshot by reading the real filesystem.
    pub(super) fn snapshot_from_fs(
        &self,
        mount_name: &str,
        path: &str,
        rp: &Path,
    ) -> Result<DirSnapshot, Errno> {
        let read_dir = std::fs::read_dir(rp).map_err(|_| Errno::EIO)?;
        let mut snapshot = Vec::new();
        for dir_entry in read_dir.flatten() {
            let fname = dir_entry.file_name();
            let Some(fname_str) = fname.to_str() else {
                continue;
            };
            let child_rp = dir_entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&child_rp) else {
                continue;
            };
            let kind = if meta.is_dir() {
                wit_types::EntryKind::Directory
            } else {
                file_kind_placeholder()
            };
            let child_path = join_child_path(path, fname_str);
            let child_ino = self.get_or_alloc_ino_backing(
                mount_name,
                &child_path,
                kind.clone(),
                meta.len(),
                child_rp,
            );
            snapshot.push((child_ino, fname_str.to_string(), kind));
        }
        Ok(snapshot)
    }

    /// Build a directory snapshot from cached dirent records.
    fn snapshot_from_dirents(
        &self,
        mount_name: &str,
        path: &str,
        dirents: &DirentsPayload,
    ) -> DirSnapshot {
        dirents
            .entries
            .iter()
            .map(|e| {
                let child_path = join_child_path(path, &e.name);
                let child_ino = self.get_or_alloc_ino_meta(mount_name, &child_path, e.meta.clone());
                (
                    child_ino,
                    e.name.clone(),
                    wit_protocol::entry_kind_to_wit(&e.meta.kind),
                )
            })
            .collect()
    }

    pub(super) fn serve_control_read(
        &self,
        mount_name: &str,
        parent_path: &str,
        leaf: &str,
        trace: Option<TraceId>,
    ) -> Option<String> {
        self.serve_synthetic_control_read(&self.rt, mount_name, parent_path, leaf, trace)
    }

    pub(super) fn opendir_check_caches(
        &self,
        mount_name: &str,
        _ino: u64,
        path: &str,
        live: Option<&InspectorFuseScope>,
        started: Instant,
    ) -> Result<Option<DirSnapshot>, Errno> {
        self.drain_and_evict_pending(mount_name);
        let elapsed = || started.elapsed();

        // Serve readdir from cache only for an authoritative listing: an
        // exhaustive record, or a host-accumulated paginated record (still
        // paging, or exhausted-but-complete). A plain non-exhaustive record is
        // a partial prefetched snapshot (e.g. from a Lookup that merged
        // route-derived structural children) and must NOT be returned as the
        // authoritative listing; the host falls through to list_children so the
        // provider enumerates fully. See `DirentsPayload::is_authoritative_listing`.
        if let Some(record) = self.mem_get(mount_name, path, RecordKind::Dirents)
            && let Some(dirents) = DirentsPayload::deserialize(&record.payload)
            && dirents.is_authoritative_listing()
        {
            debug!(target: "omnifs_cache", kind = "mem_hit", op = "opendir", mount = mount_name, "cache hit");
            if let Some(scope) = live {
                scope.emit_cache(CacheKind::BrowseHit, elapsed());
            }
            return Ok(Some(self.snapshot_from_dirents(mount_name, path, &dirents)));
        }

        // Unified cache.
        if let Some(runtime) = self.runtime_for_mount(mount_name) {
            if let Some(record) = runtime.cache_get(path, RecordKind::Dirents, None)
                && let Some(dirents) = DirentsPayload::deserialize(&record.payload)
                && dirents.is_authoritative_listing()
            {
                debug!(target: "omnifs_cache", kind = "disk_hit", op = "opendir", mount = mount_name, "cache hit");
                if let Some(scope) = live {
                    scope.emit_cache(CacheKind::BrowseHit, elapsed());
                }
                return Ok(Some(self.snapshot_from_dirents(mount_name, path, &dirents)));
            }
            // Serve-stale-while-rate-limited: while the mount's window is open,
            // serve the last-known listing (even a non-authoritative prefix)
            // rather than calling the provider and getting EAGAIN.
            if runtime.rate_limited_until().is_some()
                && let Some(dirents) =
                    self.cached_dirents_for_revalidation(mount_name, &runtime, path)
            {
                debug!(target: "omnifs_cache", kind = "stale_serve", op = "opendir", mount = mount_name, "rate-limited; serving stale listing");
                if let Some(scope) = live {
                    scope.emit_cache(CacheKind::BrowseHit, elapsed());
                }
                return Ok(Some(self.snapshot_from_dirents(mount_name, path, &dirents)));
            }
        } else {
            return Err(Errno::ENOENT);
        }

        Ok(None)
    }

    /// List directory entries through the provider and cache the result.
    ///
    /// Subtree handoff folds into the `ListChildrenResult::Subtree(..)`
    /// variant returned from the provider.
    /// The cached dirents record for `path`, exhaustive or not, used to
    /// recover the listing validator for revalidation and to serve an
    /// `unchanged` result. `mem` first, then the unified cache.
    fn cached_dirents_for_revalidation(
        &self,
        mount_name: &str,
        runtime: &Arc<Runtime>,
        path: &str,
    ) -> Option<DirentsPayload> {
        if let Some(record) = self.mem_get(mount_name, path, RecordKind::Dirents)
            && let Some(dirents) = DirentsPayload::deserialize(&record.payload)
        {
            return Some(dirents);
        }
        let record = runtime.cache_get(path, RecordKind::Dirents, None)?;
        DirentsPayload::deserialize(&record.payload)
    }

    /// Find a synthetic `@next`/`@all` dirent in the parent directory's cached
    /// dirents record (mem then unified cache). Returns `None` when the
    /// parent is not a paged directory or the control entry is absent (feed
    /// exhausted).
    pub(super) fn cached_control_dirent(
        &self,
        mount_name: &str,
        parent_path: &str,
        name: &str,
    ) -> Option<DirentRecord> {
        let dirents =
            if let Some(record) = self.mem_get(mount_name, parent_path, RecordKind::Dirents) {
                DirentsPayload::deserialize(&record.payload)?
            } else {
                let runtime = self.runtime_for_mount(mount_name)?;
                let record = runtime.cache_get(parent_path, RecordKind::Dirents, None)?;
                DirentsPayload::deserialize(&record.payload)?
            };
        dirents.entries.into_iter().find(|e| e.name == name)
    }

    /// Synthesize a mount-root ignore file (`.gitignore`/`.ignore`/`.rgignore`)
    /// after the provider has been consulted and returned no such file. Marks
    /// the resulting inode `synthetic` so `open` serves the fixed ignore content
    /// from a per-`fh` buffer instead of issuing a provider `read_file`. Returns
    /// `None` when the name is not a root ignore file, in which case the caller
    /// surfaces the original ENOENT.
    /// Reply to a lookup that resolved negatively. Synthesizes a mount-root
    /// ignore file when applicable (the provider has been consulted and has no
    /// such file), otherwise marks the scope outcome and surfaces `errno`.
    pub(super) fn reply_lookup_negative(
        &self,
        reply: ReplyEntry,
        scope: Option<&InspectorFuseScope>,
        mount_name: &str,
        parent_path: &str,
        name: &str,
        errno: Errno,
    ) {
        if let Some((attr, ttl)) = self.synthesize_root_ignore_lookup(mount_name, parent_path, name)
        {
            reply.entry(&ttl, &attr, Generation(0));
            return;
        }
        if let Some(scope) = scope {
            scope.set_outcome(inspector_outcome(errno));
        }
        reply.error(errno);
    }

    /// Insert a directory snapshot for `fh`, appending mount-root ignore files
    /// (`.gitignore`/`.ignore`/`.rgignore`) when `path` is the mount root so
    /// `rg`/`fd`/git skip the `@`-prefixed control files during tree walks.
    pub(super) fn insert_dir_snapshot(
        &self,
        fh: u64,
        mount_name: &str,
        path: &str,
        mut snapshot: DirSnapshot,
    ) {
        if is_mount_root(path) {
            for name in pagination::IGNORE_FILES {
                // The provider may already project a real ignore file at the
                // root; don't double-list it.
                if snapshot.iter().any(|(_, n, _)| n == name) {
                    continue;
                }
                let meta = root_ignore_meta();
                // Host-synthesized: mark the inode `synthetic` so `open` serves
                // the fixed ignore content from a per-`fh` buffer rather than the
                // provider. A real provider `.gitignore` is handled by the
                // snapshot branch above and never reaches here.
                let ignore_path = join_child_path(path, name);
                let ino = self.get_or_alloc_ino_synthetic(mount_name, &ignore_path, meta.clone());
                snapshot.push((
                    ino,
                    name.to_string(),
                    wit_protocol::entry_kind_to_wit(&meta.kind),
                ));
            }
        }
        self.dir_snapshots.insert(fh, snapshot);
    }

    /// Materialize a FUSE directory snapshot from a provider `list-children` listing.
    fn snapshot_from_provider_listing(
        &self,
        runtime: &Arc<Runtime>,
        mount_name: &str,
        path: &str,
        listing: &wit_types::DirListing,
    ) -> DirSnapshot {
        let dir_entries = &listing.entries;
        let mut snapshot = Vec::with_capacity(dir_entries.len());
        let mut dirent_records = Vec::with_capacity(dir_entries.len());
        for e in dir_entries {
            // `@` is reserved for host control entries: a provider must
            // never shadow `@next`/`@all`. Skip with a warning.
            if pagination::is_reserved_provider_leaf(&e.name) {
                warn!(
                    name = e.name.as_str(),
                    path, "provider listing yielded a reserved '@'-prefixed entry; skipping"
                );
                continue;
            }
            let child_path = join_child_path(path, &e.name);
            let meta = wit_protocol::entry_meta_from_kind(&e.kind);
            // A fresh provider listing authoritatively resolves each
            // child; a real entry here clears any prior synthetic marker.
            let child_ino =
                self.get_or_alloc_ino_meta_resolved(mount_name, &child_path, meta.clone());
            snapshot.push((
                child_ino,
                e.name.clone(),
                wit_protocol::entry_kind_to_wit(&meta.kind),
            ));
            dirent_records.push(DirentRecord {
                name: e.name.clone(),
                meta,
            });
        }
        let next_cursor = listing
            .next_cursor
            .clone()
            .map(wit_protocol::cached_cursor_from_wit);
        let paginated = next_cursor.is_some();
        if paginated {
            self.append_synthetic_control_entries(
                mount_name,
                path,
                &mut snapshot,
                &mut dirent_records,
            );
        }
        let dirents_payload = DirentsPayload {
            entries: dirent_records,
            // A paged listing is never exhaustive while a cursor remains.
            exhaustive: listing.exhaustive && next_cursor.is_none(),
            validator: listing.validator.clone(),
            next_cursor,
            paginated,
        };
        if let Some(encoded) = dirents_payload.serialize() {
            let dirents_record = CacheRecord::new(RecordKind::Dirents, encoded);
            runtime.cache_put(path, RecordKind::Dirents, None, &dirents_record);
        }
        snapshot
    }

    pub(super) fn opendir_via_provider(
        &self,
        runtime: &Arc<Runtime>,
        mount_name: &str,
        ino: u64,
        path: &str,
        fuse_trace: Option<TraceId>,
    ) -> Result<DirSnapshot, Errno> {
        // A non-exhaustive cached dirents record may carry a listing
        // validator the provider can revalidate against (OPEN-8). Echo it
        // so the provider can answer `unchanged`.
        let cached_dirents = self.cached_dirents_for_revalidation(mount_name, runtime, path);
        let cached_validator = cached_dirents.as_ref().and_then(|d| d.validator.clone());
        // Plain readdir serves the first page only; no cursor to resume.
        match self.rt.block_on(runtime.namespace().list_children(
            path,
            cached_validator,
            None,
            fuse_trace,
        )) {
            Ok(ListChildrenResult::Unchanged) => {
                let Some(dirents) = cached_dirents else {
                    // The provider reported `unchanged` but the host holds
                    // no cached listing to serve. Treat as a protocol miss.
                    warn!(
                        path,
                        "list_children returned unchanged with no cached listing"
                    );
                    return Err(Errno::EIO);
                };
                Ok(self.snapshot_from_dirents(mount_name, path, &dirents))
            },
            Ok(ListChildrenResult::Subtree(tree_ref)) => {
                let Some(real_root) = runtime.resolve_tree_ref(tree_ref) else {
                    return Err(Errno::EIO);
                };
                if let Some(mut entry) = self.inodes.get_mut(&ino)
                    && entry.backing_path.is_none()
                {
                    entry.backing_path = Some(real_root.clone());
                }
                self.snapshot_from_fs(mount_name, path, &real_root)
            },
            Ok(ListChildrenResult::Entries(listing)) => {
                Ok(self.snapshot_from_provider_listing(runtime, mount_name, path, &listing))
            },
            Err(Error::ProviderError(error)) => {
                warn!(
                    path,
                    kind = ?error.kind,
                    retryable = error.retryable,
                    message = error.message,
                    "provider returned typed error for list_children"
                );
                // Serve stale so `ls` survives upstream throttling.
                if error.kind == ErrorKind::RateLimited
                    && let Some(dirents) = cached_dirents
                {
                    return Ok(self.snapshot_from_dirents(mount_name, path, &dirents));
                }
                Err(super::errno::provider_error_errno(&error))
            },
            Err(error) => {
                warn!(
                    path,
                    error = %error,
                    "list_children runtime error"
                );
                Err(Errno::EIO)
            },
        }
    }
}
