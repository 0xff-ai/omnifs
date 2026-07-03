//! `fuser::Filesystem` trait implementation for [`super::Frontend`].

use super::Frontend;
use super::common::{FullReadTarget, ROOT_INO, TTL};
use super::errno::inspector_outcome;
use super::read_helpers::data_slice;
use super::trace::FuseTrace;
use fuser::{
    Errno, FileHandle as FuseFileHandle, Filesystem, FopenFlags, Generation, INodeNo, LockOwner,
    OpenFlags, ReplyAttr, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, Request,
};
use omnifs_api::events::CacheKind;
use omnifs_core::path::Path;
use omnifs_engine::view::EntryKind;
use omnifs_engine::{InspectorRequestScope, ListOutcome, RequestCtx, global as inspector_global};
use std::ffi::OsStr;
use std::time::Duration;
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
            let Ok(path) = Path::root().join(name_str) else {
                reply.error(Errno::EINVAL);
                return;
            };
            let ctx = RequestCtx::default();
            match self.rt.block_on(self.tree.resolve(&path, &ctx)) {
                Ok(node) => {
                    let (attr, ttl) = self.inode_attr_for_node(node.mount(), &node);
                    reply.entry(&ttl, &attr, Generation(0));
                },
                Err(_) => reply.error(Errno::ENOENT),
            }
            return;
        }

        let Some(parent_entry) = self.inodes.get(&parent.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let mount_name = parent_entry.mount_name.clone();
        let parent_path = parent_entry.path.clone();
        let parent_backing_path = parent_entry.body.backing_path().cloned();
        drop(parent_entry);

        let child_path = parent_path
            .join(name_str)
            .expect("lookup name must be a valid path segment");
        let live_scope = inspector_global().map(|sink| {
            InspectorRequestScope::begin(sink, "lookup", &mount_name, child_path.as_str())
        });
        let live = live_scope.as_ref();

        // If the parent has a backing path, resolve the child from the filesystem.
        if let Some(ref parent_rp) = parent_backing_path {
            let child_rp = parent_rp.join(name_str);
            match std::fs::symlink_metadata(&child_rp) {
                Ok(meta) => {
                    let kind = if meta.is_dir() {
                        EntryKind::Directory
                    } else {
                        EntryKind::File
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

        // Enter the async runtime once: `Tree::resolve_child` owns the cache
        // consult, the provider lookup, the `@next`/`@all` control resolution,
        // and the mount-root ignore synthesis.
        match self.lookup_op(
            &mount_name,
            &parent_path,
            name_str,
            live.map(InspectorRequestScope::trace_id),
        ) {
            Ok((attr, ttl)) => reply.entry(&ttl, &attr, Generation(0)),
            Err(errno) => {
                if let Some(scope) = live {
                    scope.set_outcome(inspector_outcome(errno));
                }
                reply.error(errno);
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

        // Passthrough for inodes backed by the local filesystem.
        if let Some(rp) = entry.body.backing_path() {
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
            EntryKind::Directory => self.dir_attr(ino.0),
            EntryKind::File => {
                let size = entry
                    .size
                    .max(self.follow_sizes.get(&ino.0).map_or(0, |v| *v));
                self.file_attr(ino.0, size)
            },
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
            let ctx = RequestCtx::default();
            match self.rt.block_on(async {
                let root = self.tree.resolve(&Path::root(), &ctx).await?;
                self.tree.list(&root, None, &ctx).await
            }) {
                Ok(ListOutcome::Listing(listing)) => {
                    let snapshot = self.snapshot_from_listing("", &Path::root(), &listing);
                    self.dir_snapshots.insert(fh, snapshot);
                    reply.opened(FuseFileHandle(fh), FopenFlags::empty());
                },
                Ok(ListOutcome::Subtree(_)) => reply.error(Errno::EIO),
                Err(_) => reply.error(Errno::ENOENT),
            }
            return;
        }

        let Some(inode_entry) = self.inodes.get(&ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let mount_name = inode_entry.mount_name.clone();
        let path = inode_entry.path.clone();
        let backing_path = inode_entry.body.backing_path().cloned();
        drop(inode_entry);

        let live_scope = inspector_global().map(|sink| {
            InspectorRequestScope::begin(sink, "opendir", &mount_name, path.to_string())
        });
        let live = live_scope.as_ref();

        // Passthrough for inodes with backing_path.
        if let Some(ref rp) = backing_path {
            match self.snapshot_from_fs(&mount_name, &path, rp) {
                Ok(snapshot) => {
                    self.dir_snapshots.insert(fh, snapshot);
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

        // Enter the async runtime once: `Tree::list` owns the cache consult,
        // the cold provider listing + cache-populate, the serve-stale path, and
        // the host-synthesized control / ignore entries in the returned snapshot.
        match self.opendir_op(
            &mount_name,
            ino.0,
            &path,
            live.map(InspectorRequestScope::trace_id),
        ) {
            Ok(snapshot) => {
                self.dir_snapshots.insert(fh, snapshot);
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
                EntryKind::Directory => fuser::FileType::Directory,
                EntryKind::File => fuser::FileType::RegularFile,
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

        let live_scope = inspector_global()
            .map(|sink| InspectorRequestScope::begin(sink, "read", &mount_name, path.to_string()));
        let live = live_scope.as_ref();

        // Host-synthetic control (`@next`/`@all`) and mount-root ignore files
        // are materialized into the per-`fh` file cache at `open` time, so they
        // are served by the cache hit below. `read` never re-runs the (mutating)
        // pagination action, so a partial or repeated read cannot advance the
        // feed more than once, and never serves `@*` content by path.

        if self.ranged_handles.contains_key(&fh.0) {
            self.read_ranged_handle(ino.0, fh.0, offset, size, reply);
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
        let body = entry.body.clone();
        let attrs = entry.attrs.clone();
        drop(entry);

        let target = FullReadTarget {
            ino: ino.0,
            fh,
            mount_name,
            path,
            body,
            attrs,
        };

        let live_scope = inspector_global().map(|sink| {
            InspectorRequestScope::begin(sink, "open", &target.mount_name, target.path.to_string())
        });
        let live = live_scope.as_ref();
        let fuse_trace = live.map(InspectorRequestScope::trace_id);

        // Enter the async runtime once: `open_op` dispatches the synthetic /
        // ranged / full-prefetch / lazy cases on the inode's projection, binding
        // a `Tree` `RangedHandle` or filling the per-`fh` buffer as needed.
        match self.open_op(&target, fuse_trace) {
            Ok(flags) => reply.opened(FuseFileHandle(fh), flags),
            Err(errno) => {
                if let Some(scope) = live {
                    scope.set_outcome(inspector_outcome(errno));
                }
                reply.error(errno);
            },
        }
    }

    fn release(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FuseFileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let _trace = FuseTrace::new("release", format!("fh={}", fh.0));
        self.file_cache.remove(&fh.0);
        if let Some((_, pump)) = self.follow_pumps.remove(&fh.0) {
            pump.abort();
        }
        if let Some((_, slot)) = self.ranged_handles.remove(&fh.0) {
            let path = slot.handle.path().to_string();
            if let Err(e) = slot.handle.close() {
                debug!(path, error = %e, "close_file error");
            }
        }
        if !self.ranged_handles.iter().any(|entry| entry.ino == ino.0) {
            self.follow_sizes.remove(&ino.0);
        }
        reply.ok();
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        let _trace = FuseTrace::new("readlink", format!("ino={}", ino.0));
        let Some(entry) = self.inodes.get(&ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        if let Some(rp) = entry.body.backing_path() {
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
