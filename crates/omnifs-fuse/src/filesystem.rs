//! `fuser::Filesystem` trait implementation for [`super::Frontend`].

use super::Frontend;
use super::common::{FullReadTarget, ROOT_INO, TTL, file_kind_placeholder, join_child_path};
use super::errno::inspector_outcome;
use super::read_helpers::data_slice;
use super::trace::FuseTrace;
use fuser::{
    Errno, FileHandle as FuseFileHandle, Filesystem, FopenFlags, Generation, INodeNo, LockOwner,
    OpenFlags, ReplyAttr, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, Request,
};
use omnifs_core::view::FileAttrsCache;
use omnifs_host::inspector::{self, InspectorFuseScope};
use omnifs_inspector::CacheKind;
use omnifs_wit::provider::types as wit_types;
use std::ffi::OsStr;
use std::time::{Duration, Instant};
use tracing::{debug, debug_span, warn};

impl Filesystem for Frontend {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some(name_str) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let _trace = FuseTrace::new("lookup", format!("parent={} name={}", parent.0, name_str));
        let _span = debug_span!("fuse::lookup", parent = parent.0, name = name_str).entered();

        let root_mount = (parent.0 == ROOT_INO).then(|| self.sync_root_mount());
        // Synthetic root (no root_mount): mount points are children.
        if root_mount.as_ref().is_some_and(Option::is_none) {
            if self.registry.get(name_str).is_some() {
                let ino = self.get_or_alloc_ino(
                    name_str,
                    omnifs_core::path::Path::ROOT,
                    wit_types::EntryKind::Directory,
                    0,
                );
                reply.entry(&TTL, &self.dir_attr(ino), Generation(0));
                return;
            }
            reply.error(Errno::ENOENT);
            return;
        }

        let Some(parent_entry) = self.inodes.get(&parent.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let mount_name = parent_entry.mount_name.clone();
        let parent_path = parent_entry.path.clone();
        let parent_backing_path = parent_entry.backing_path.clone();
        drop(parent_entry);

        let child_path = join_child_path(&parent_path, name_str);
        let live_scope = inspector::global()
            .map(|sink| InspectorFuseScope::begin(sink, "lookup", &mount_name, &child_path));
        let live = live_scope.as_ref();
        let cache_started = Instant::now();

        // If the parent has a backing path, resolve the child from the filesystem.
        if let Some(ref parent_rp) = parent_backing_path {
            let child_rp = parent_rp.join(name_str);
            match std::fs::symlink_metadata(&child_rp) {
                Ok(meta) => {
                    let kind = if meta.is_dir() {
                        wit_types::EntryKind::Directory
                    } else {
                        file_kind_placeholder()
                    };
                    let ino = self.get_or_alloc_ino_backing(
                        &mount_name,
                        &child_path,
                        kind,
                        meta.len(),
                        child_rp,
                    );
                    reply.entry(&TTL, &self.attr_from_metadata(ino, &meta), Generation(0));
                },
                Err(e) => {
                    warn!(path = ?child_rp, err = %e, "backing fs error");
                    reply.error(Errno::ENOENT);
                },
            }
            return;
        }

        // view cache path.
        match self.lookup_check_caches(&mount_name, &parent_path, name_str, live, cache_started) {
            Ok(Some((attr, ttl))) => {
                reply.entry(&ttl, &attr, Generation(0));
                return;
            },
            Err(e) => {
                // A cache-implied negative is authoritative for the provider's
                // listing; a root ignore file the provider does not project is
                // host-synthesized here, after the negative.
                self.reply_lookup_negative(reply, live, &mount_name, &parent_path, name_str, e);
                return;
            },
            Ok(None) => {},
        }

        let Some(runtime) = self.runtime_for_mount(&mount_name) else {
            // No runtime, but a root ignore file is still host-synthesized.
            self.reply_lookup_negative(
                reply,
                live,
                &mount_name,
                &parent_path,
                name_str,
                Errno::ENOENT,
            );
            return;
        };

        debug!(target: "omnifs_cache", kind = "miss", op = "lookup", mount = mount_name.as_str(), "cache miss");

        match self.lookup_via_provider(
            &runtime,
            &mount_name,
            &parent_path,
            name_str,
            live.map(InspectorFuseScope::trace_id),
        ) {
            Ok((attr, ttl)) => reply.entry(&ttl, &attr, Generation(0)),
            Err(errno) => {
                // The provider has no such child; a root ignore file is
                // host-synthesized only now, never shadowing a real one.
                self.reply_lookup_negative(reply, live, &mount_name, &parent_path, name_str, errno);
            },
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FuseFileHandle>, reply: ReplyAttr) {
        let _trace = FuseTrace::new("getattr", format!("ino={}", ino.0));
        if ino.0 == ROOT_INO {
            reply.attr(&TTL, &self.dir_attr(ROOT_INO));
            return;
        }

        let Some(entry) = self.inodes.get(&ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };

        // Passthrough for inodes with backing_path.
        if let Some(ref rp) = entry.backing_path {
            match std::fs::symlink_metadata(rp) {
                Ok(meta) => {
                    let attr = self.attr_from_metadata(ino.0, &meta);
                    reply.attr(&TTL, &attr);
                },
                Err(e) => {
                    warn!(path = ?rp, err = %e, "backing fs error");
                    reply.error(Errno::ENOENT);
                },
            }
            return;
        }

        let attr = match &entry.kind {
            wit_types::EntryKind::Directory => self.dir_attr(ino.0),
            wit_types::EntryKind::File(_) => self.file_attr(ino.0, entry.size),
        };
        let ttl = Self::ttl_for_entry(&entry);
        reply.attr(&ttl, &attr);
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let _trace = FuseTrace::new("opendir", format!("ino={}", ino.0));
        let _span = debug_span!("fuse::opendir", inode = ino.0).entered();

        let fh = self.alloc_fh();

        let root_mount = (ino.0 == ROOT_INO).then(|| self.sync_root_mount());
        // Synthetic root (no root_mount): list mount points.
        if root_mount.as_ref().is_some_and(Option::is_none) {
            let mounts = self.registry.mounts();
            let mut entries = Vec::new();
            for m in mounts {
                let child_ino = self.get_or_alloc_ino(&m, "", wit_types::EntryKind::Directory, 0);
                entries.push((child_ino, m, wit_types::EntryKind::Directory));
            }
            self.dir_snapshots.insert(fh, entries);
            reply.opened(FuseFileHandle(fh), FopenFlags::empty());
            return;
        }

        let Some(inode_entry) = self.inodes.get(&ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let mount_name = inode_entry.mount_name.clone();
        let path = inode_entry.path.clone();
        let backing_path = inode_entry.backing_path.clone();
        drop(inode_entry);

        let live_scope = inspector::global()
            .map(|sink| InspectorFuseScope::begin(sink, "opendir", &mount_name, &path));
        let live = live_scope.as_ref();
        let cache_started = Instant::now();

        // Passthrough for inodes with backing_path.
        if let Some(ref rp) = backing_path {
            match self.snapshot_from_fs(&mount_name, &path, rp) {
                Ok(snapshot) => {
                    self.insert_dir_snapshot(fh, &mount_name, &path, snapshot);
                    reply.opened(FuseFileHandle(fh), FopenFlags::empty());
                },
                Err(e) => {
                    if let Some(scope) = &live_scope {
                        scope.set_outcome(inspector_outcome(e));
                    }
                    reply.error(e);
                },
            }
            return;
        }

        // view cache path.
        match self.opendir_check_caches(&mount_name, ino.0, &path, live, cache_started) {
            Ok(Some(snapshot)) => {
                self.insert_dir_snapshot(fh, &mount_name, &path, snapshot);
                reply.opened(FuseFileHandle(fh), FopenFlags::empty());
                return;
            },
            Err(e) => {
                if let Some(scope) = &live_scope {
                    scope.set_outcome(inspector_outcome(e));
                }
                reply.error(e);
                return;
            },
            Ok(None) => {},
        }

        self.drain_and_evict_pending(&mount_name);

        let Some(runtime) = self.runtime_for_mount(&mount_name) else {
            if let Some(scope) = &live_scope {
                scope.set_outcome(inspector_outcome(Errno::ENOENT));
            }
            reply.error(Errno::ENOENT);
            return;
        };

        debug!(target: "omnifs_cache", kind = "miss", op = "opendir", mount = mount_name.as_str(), "cache miss");

        match self.opendir_via_provider(
            &runtime,
            &mount_name,
            ino.0,
            &path,
            live.map(InspectorFuseScope::trace_id),
        ) {
            Ok(snapshot) => {
                self.insert_dir_snapshot(fh, &mount_name, &path, snapshot);
                reply.opened(FuseFileHandle(fh), FopenFlags::empty());
            },
            Err(e) => {
                if let Some(scope) = &live_scope {
                    scope.set_outcome(inspector_outcome(e));
                }
                reply.error(e);
            },
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FuseFileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let _trace = FuseTrace::new("readdir", format!("fh={} offset={}", fh.0, offset));
        let Some(snapshot) = self.dir_snapshots.get(&fh.0) else {
            reply.error(Errno::EBADF);
            return;
        };

        #[allow(clippy::cast_possible_truncation)]
        let skip = offset as usize;
        for (index, (ino, name, kind)) in snapshot.iter().enumerate().skip(skip) {
            let ftype = match kind {
                wit_types::EntryKind::Directory => fuser::FileType::Directory,
                wit_types::EntryKind::File(_) => fuser::FileType::RegularFile,
            };
            let buffer_full = reply.add(INodeNo(*ino), (index + 1) as u64, ftype, name.as_str());
            if buffer_full {
                break;
            }
        }
        reply.ok();
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FuseFileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        let _trace = FuseTrace::new("releasedir", format!("fh={}", fh.0));
        self.dir_snapshots.remove(&fh.0);
        reply.ok();
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FuseFileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let _trace = FuseTrace::new(
            "read",
            format!("ino={} fh={} offset={} size={}", ino.0, fh.0, offset, size),
        );
        let _span = debug_span!("fuse::read", inode = ino.0, offset, size).entered();

        let Some(inode_entry) = self.inodes.get(&ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let mount_name = inode_entry.mount_name.clone();
        let path = inode_entry.path.clone();
        drop(inode_entry);

        let live_scope = inspector::global()
            .map(|sink| InspectorFuseScope::begin(sink, "read", &mount_name, &path));
        let live = live_scope.as_ref();

        // Host-synthetic control (`@next`/`@all`) and mount-root ignore files
        // are materialized into the per-`fh` file cache at `open` time, so they
        // are served by the cache hit below. `read` never re-runs the (mutating)
        // pagination action, so a partial or repeated read cannot advance the
        // feed more than once, and never serves `@*` content by path.

        if let Some(ranged) = self.ranged_handles.get(&fh.0).map(|entry| entry.clone()) {
            self.read_ranged_handle(ino.0, &ranged, offset, size, reply);
            return;
        }

        // Serve from cache if this file handle already has data.
        if let Some(cached) = self.file_cache.get(&fh.0) {
            if let Some(scope) = live {
                scope.emit_cache(CacheKind::FileHit, Duration::ZERO);
            }
            reply.data(data_slice(&cached, offset, size));
            return;
        }

        self.read_full_handle(ino, fh, offset, size, live, reply);
    }

    fn open(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let _trace = FuseTrace::new("open", format!("ino={}", ino.0));
        let fh = self.alloc_fh();
        let Some(entry) = self.inodes.get(&ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let mount_name = entry.mount_name.clone();
        let path = entry.path.clone();
        let backing_path = entry.backing_path.clone();
        let attrs = entry.attrs.clone();
        let synthetic = entry.synthetic;
        drop(entry);

        let target = FullReadTarget {
            ino: ino.0,
            fh,
            mount_name,
            path,
            backing_path,
            attrs,
            synthetic,
        };

        let live_scope = inspector::global()
            .map(|sink| InspectorFuseScope::begin(sink, "open", &target.mount_name, &target.path));
        let live = live_scope.as_ref();
        let fuse_trace = live.map(InspectorFuseScope::trace_id);

        // Host-synthetic control/ignore files are served from a per-`fh` buffer,
        // never through the provider read/prefetch path.
        match self.open_synthetic_file(&target, fuse_trace) {
            Ok(Some(flags)) => {
                reply.opened(FuseFileHandle(fh), flags);
                return;
            },
            Ok(None) => {},
            Err(errno) => {
                if let Some(scope) = live {
                    scope.set_outcome(inspector_outcome(errno));
                }
                reply.error(errno);
                return;
            },
        }

        match self.open_ranged_file(&target) {
            Ok(Some(flags)) => {
                reply.opened(FuseFileHandle(fh), flags);
                return;
            },
            Ok(None) => {},
            Err(errno) => {
                if let Some(scope) = live {
                    scope.set_outcome(inspector_outcome(errno));
                }
                reply.error(errno);
                return;
            },
        }

        match self.prefetch_full_file_on_open(&target, fuse_trace) {
            Ok(Some(flags)) => {
                reply.opened(FuseFileHandle(fh), flags);
                return;
            },
            Ok(None) => {},
            Err(errno) => {
                if let Some(scope) = live {
                    scope.set_outcome(inspector_outcome(errno));
                }
                reply.error(errno);
                return;
            },
        }

        let flags = target
            .attrs
            .filter(FileAttrsCache::should_direct_io)
            .map_or_else(FopenFlags::empty, |_| FopenFlags::FOPEN_DIRECT_IO);
        reply.opened(FuseFileHandle(fh), flags);
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FuseFileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let _trace = FuseTrace::new("release", format!("fh={}", fh.0));
        self.file_cache.remove(&fh.0);
        if let Some((_, ranged)) = self.ranged_handles.remove(&fh.0)
            && let Some(runtime) = self.runtime_for_mount(&ranged.mount_name)
            && let Err(e) = runtime.call_close_file(ranged.provider_handle)
        {
            debug!(
                path = ranged.path,
                error = %e,
                "close_file runtime error"
            );
        }
        reply.ok();
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        let _trace = FuseTrace::new("readlink", format!("ino={}", ino.0));
        let Some(entry) = self.inodes.get(&ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        if let Some(ref rp) = entry.backing_path {
            match std::fs::read_link(rp) {
                Ok(target) => reply.data(target.as_os_str().as_encoded_bytes()),
                Err(e) => {
                    warn!(path = ?rp, err = %e, "backing fs error");
                    reply.error(Errno::EIO);
                },
            }
        } else {
            reply.error(Errno::EINVAL);
        }
    }
}
