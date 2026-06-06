//! Provider and cache lookup for FUSE `lookup`.

use super::Frontend;
use super::common::{TTL, is_mount_root, join_child_path, root_ignore_meta};
use fuser::{Errno, FileAttr};
use omnifs_cache::RecordKind;
use omnifs_core::view::{DirentsPayload, LookupPayload};
use omnifs_host::inspector::InspectorFuseScope;
use omnifs_host::path_key::PathKey;
use omnifs_host::wit_protocol;
use omnifs_host::{Error, LookupOutcome, Runtime, pagination};
use omnifs_inspector::{CacheKind, TraceId};
use omnifs_wit::provider::types as wit_types;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, warn};

impl Frontend {
    fn resolve_lookup_hit(
        &self,
        mount_name: &str,
        child_path: &str,
        lookup: &LookupPayload,
        tier: &str,
    ) -> Result<(FileAttr, Duration), Errno> {
        match lookup {
            LookupPayload::Negative => {
                debug!(target: "omnifs_cache", kind = "negative_hit", op = "lookup", mount = mount_name, "negative cache hit");
                Err(Errno::ENOENT)
            },
            LookupPayload::Positive(meta) => {
                debug!(target: "omnifs_cache", kind = tier, op = "lookup", mount = mount_name, "cache hit");
                let ino = self.get_or_alloc_ino_meta(mount_name, child_path, meta.clone());
                let kind = wit_protocol::entry_kind_to_wit(&meta.kind);
                Ok((
                    self.attr_for_inode_or_meta(ino, &kind, meta.st_size()),
                    Self::ttl_for_meta(meta),
                ))
            },
        }
    }
    pub(super) fn lookup_check_caches(
        &self,
        mount_name: &str,
        parent_path: &str,
        name_str: &str,
        live: Option<&InspectorFuseScope>,
        started: Instant,
    ) -> Result<Option<(FileAttr, Duration)>, Errno> {
        let child_path = join_child_path(parent_path, name_str);
        let elapsed = || started.elapsed();
        self.drain_and_evict_pending(mount_name);

        // Synthetic `@next`/`@all` controls resolve *only* from the parent's
        // cached dirents record, which carries them while a resume cursor
        // remains. A reserved control name is never a real provider entry, so
        // once the control is gone (feed exhausted, cursor cleared) the lookup
        // is ENOENT: we must not fall through to the `path_to_inode` dedup
        // table, which would resurrect a stale `@next`/`@all` inode.
        match self.lookup_synthetic_control(mount_name, parent_path, name_str) {
            Ok(Some(hit)) => {
                if let Some(scope) = live {
                    scope.emit_cache(CacheKind::BrowseHit, elapsed());
                }
                return Ok(Some(hit));
            },
            Ok(None) => {},
            Err(e) => {
                if pagination::is_control_name(name_str)
                    && let Some(scope) = live
                {
                    scope.emit_cache(CacheKind::BrowseMiss, elapsed());
                }
                return Err(e);
            },
        }

        // Mount-root ignore files (`.gitignore`/`.ignore`/...) are NOT
        // synthesized here. The provider may project a real one, so resolution
        // runs through the normal cache/provider lookup below; only a negative
        // provider result synthesizes the host ignore file (see `lookup`).

        // Dirents-implied negative: if parent dirents are cached and
        // exhaustive, trust the cache.
        if let Some(record) = self.mem_get(mount_name, parent_path, RecordKind::Dirents)
            && let Some(dirents) = DirentsPayload::deserialize(&record.payload)
            && dirents.exhaustive
        {
            if let Some(dirent) = dirents.entries.iter().find(|e| e.name == name_str) {
                if let Some(scope) = live {
                    scope.emit_cache(CacheKind::BrowseHit, elapsed());
                }
                let ino = self.get_or_alloc_ino_meta(mount_name, &child_path, dirent.meta.clone());
                let kind = wit_protocol::entry_kind_to_wit(&dirent.meta.kind);
                return Ok(Some((
                    self.attr_for_inode_or_meta(ino, &kind, dirent.meta.st_size()),
                    Self::ttl_for_meta(&dirent.meta),
                )));
            }
            if let Some(scope) = live {
                scope.emit_cache(CacheKind::BrowseMiss, elapsed());
            }
            return Err(Errno::ENOENT);
        }

        // Mem: check cached lookup by child path.
        if let Some(record) = self.mem_get(mount_name, &child_path, RecordKind::Lookup)
            && let Some(lookup) = LookupPayload::deserialize(&record.payload)
        {
            if let Some(scope) = live {
                let kind = match lookup {
                    LookupPayload::Negative => CacheKind::BrowseMiss,
                    LookupPayload::Positive(_) => CacheKind::BrowseHit,
                };
                scope.emit_cache(kind, elapsed());
            }
            return self
                .resolve_lookup_hit(mount_name, &child_path, &lookup, "mem_hit")
                .map(Some);
        }

        // Unified cache: check cached lookup by path (needs runtime).
        let Some(runtime) = self.runtime_for_mount(mount_name) else {
            return Err(Errno::ENOENT);
        };
        if let Some(record) = runtime.cache_get(&child_path, RecordKind::Lookup, None)
            && let Some(lookup) = LookupPayload::deserialize(&record.payload)
        {
            if let Some(scope) = live {
                let kind = match lookup {
                    LookupPayload::Negative => CacheKind::BrowseMiss,
                    LookupPayload::Positive(_) => CacheKind::BrowseHit,
                };
                scope.emit_cache(kind, elapsed());
            }
            return self
                .resolve_lookup_hit(mount_name, &child_path, &lookup, "disk_hit")
                .map(Some);
        }

        // Drain invalidations and check the dedup table.
        self.drain_and_evict_pending(mount_name);
        let child_key = PathKey::new(mount_name, &child_path);
        if let Some(ino_ref) = self.path_to_inode.get(&child_key) {
            let ino = *ino_ref;
            drop(ino_ref);
            if let Some(entry) = self.inodes.get(&ino) {
                // A still-synthetic mount-root ignore inode must NOT short-
                // circuit provider consultation: the provider may project a real
                // `.gitignore` at the root, which has to win. Fall through to the
                // provider lookup; only after a negative result does
                // `reply_lookup_negative` re-synthesize the host ignore file.
                if Frontend::skip_dedup_for_root_ignore(entry.synthetic, parent_path, name_str) {
                    return Ok(None);
                }
                if let Some(scope) = live {
                    scope.emit_cache(CacheKind::BrowseHit, elapsed());
                }
                let attr = self.attr_for_kind(ino, &entry.kind, entry.size);
                let ttl = Self::ttl_for_entry(&entry);
                return Ok(Some((attr, ttl)));
            }
        }

        Ok(None)
    }

    /// Perform a provider-delegated lookup and write results through to caches.
    pub(super) fn lookup_via_provider(
        &self,
        runtime: &Arc<Runtime>,
        mount_name: &str,
        parent_path: &str,
        name_str: &str,
        fuse_trace: Option<TraceId>,
    ) -> Result<(FileAttr, Duration), Errno> {
        let child_path = join_child_path(parent_path, name_str);

        match self.rt.block_on(
            runtime
                .namespace()
                .lookup_child(parent_path, name_str, fuse_trace),
        ) {
            Ok(LookupOutcome::Subtree(tree_ref)) => {
                let Some(real_root) = runtime.resolve_tree_ref(tree_ref) else {
                    return Err(Errno::EIO);
                };
                let ino = self.get_or_alloc_ino_backing(
                    mount_name,
                    &child_path,
                    wit_types::EntryKind::Directory,
                    0,
                    real_root,
                );
                Ok((self.dir_attr(ino), TTL))
            },
            Ok(LookupOutcome::Entry(entry)) => {
                debug!(
                    target: "omnifs_lookup",
                    path = entry.path().as_str(),
                    "received Lookup entry"
                );

                let meta = entry.meta().clone();
                let size = meta.st_size();
                let kind = wit_protocol::entry_kind_to_wit(&meta.kind);
                let ttl = Self::ttl_for_meta(&meta);
                // A genuine provider resolution: a real file here wins over any
                // prior synthetic marker (e.g. a provider `.gitignore`).
                let ino =
                    self.get_or_alloc_ino_meta_resolved(mount_name, entry.path(), meta.clone());
                Ok((self.attr_for_inode_or_meta(ino, &kind, size), ttl))
            },
            Ok(LookupOutcome::NotFound) => Err(Errno::ENOENT),
            Err(Error::ProviderError(error)) => {
                warn!(
                    path = child_path,
                    kind = ?error.kind,
                    retryable = error.retryable,
                    message = error.message,
                    "provider returned typed error for lookup_child"
                );
                Err(super::errno::provider_error_errno(&error))
            },
            Err(error) => {
                warn!(
                    path = child_path,
                    error = %error,
                    "lookup_child runtime error"
                );
                Err(Errno::EIO)
            },
        }
    }
    pub(super) fn synthesize_root_ignore_lookup(
        &self,
        mount_name: &str,
        parent_path: &str,
        name: &str,
    ) -> Option<(FileAttr, Duration)> {
        if !is_mount_root(parent_path) || !pagination::is_ignore_name(name) {
            return None;
        }
        let child_path = join_child_path(parent_path, name);
        let meta = root_ignore_meta();
        let ino = self.get_or_alloc_ino_synthetic(mount_name, &child_path, meta.clone());
        let kind = wit_protocol::entry_kind_to_wit(&meta.kind);
        Some((
            self.attr_for_inode_or_meta(ino, &kind, meta.st_size()),
            Self::ttl_for_meta(&meta),
        ))
    }
}
