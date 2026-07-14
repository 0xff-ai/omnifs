//! `fuser::Filesystem` trait implementation for [`super::Frontend`].
//!
//! Every callback is a thin dispatch: clone the frontend, spawn the async op
//! onto the runtime (so the fuser event-loop thread never blocks and notifier
//! calls never deadlock the dispatch thread), then marshal the op's plain-data
//! result into the kernel `Reply*` sink. All resolution/attr/read decisions live
//! in `ops.rs`.

use super::Frontend;
use fuser::{
    Errno, FileHandle as FuseFileHandle, Filesystem, FopenFlags, Generation, INodeNo, LockOwner,
    OpenFlags, ReplyAttr, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, Request,
};
use std::ffi::OsStr;
use tracing::{Instrument, debug_span};

impl Filesystem for Frontend {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let fs = self.clone();
        let name = name.to_owned();
        let span = debug_span!("fuse::lookup", parent = parent.0);
        drop(
            self.rt.spawn(
                async move {
                    let Some(name_str) = name.to_str() else {
                        reply.error(Errno::EINVAL);
                        return;
                    };
                    match fs.do_lookup(parent.0, name_str).await {
                        Ok((_ino, attr, ttl)) => reply.entry(&ttl, &attr, Generation(0)),
                        Err(errno) => reply.error(errno),
                    }
                }
                .instrument(span),
            ),
        );
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FuseFileHandle>, reply: ReplyAttr) {
        let fs = self.clone();
        drop(self.rt.spawn(async move {
            match fs.do_getattr(ino.0).await {
                Ok((attr, ttl)) => reply.attr(&ttl, &attr),
                Err(errno) => reply.error(errno),
            }
        }));
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let fs = self.clone();
        let span = debug_span!("fuse::opendir", inode = ino.0);
        drop(
            self.rt.spawn(
                async move {
                    let fh = fs.alloc_fh();
                    match fs.do_opendir(ino.0).await {
                        Ok(snapshot) => {
                            fs.dir_snapshots.insert(fh, snapshot);
                            reply.opened(FuseFileHandle(fh), FopenFlags::empty());
                        },
                        Err(errno) => reply.error(errno),
                    }
                }
                .instrument(span),
            ),
        );
    }

    fn readdir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FuseFileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let fs = self.clone();
        drop(self.rt.spawn(async move {
            let Some(snapshot) = fs.dir_snapshots.get(&fh.0) else {
                reply.error(Errno::EBADF);
                return;
            };
            #[allow(clippy::cast_possible_truncation)]
            let skip = offset as usize;
            for (index, (ino, name, kind)) in snapshot.iter().enumerate().skip(skip) {
                let buffer_full = reply.add(
                    INodeNo(*ino),
                    (index + 1) as u64,
                    kind.file_type(),
                    name.as_str(),
                );
                if buffer_full {
                    break;
                }
            }
            reply.ok();
        }));
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FuseFileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        let fs = self.clone();
        drop(self.rt.spawn(async move {
            fs.do_releasedir(fh.0);
            reply.ok();
        }));
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
        let fs = self.clone();
        let span = debug_span!("fuse::read", inode = ino.0, offset, size);
        drop(
            self.rt.spawn(
                async move {
                    match fs.do_read(ino.0, fh.0, offset, size).await {
                        Ok(bytes) => reply.data(&bytes),
                        Err(errno) => reply.error(errno),
                    }
                }
                .instrument(span),
            ),
        );
    }

    fn open(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let fs = self.clone();
        drop(self.rt.spawn(async move {
            let fh = fs.alloc_fh();
            match fs.do_open(ino.0, fh).await {
                Ok(flags) => reply.opened(FuseFileHandle(fh), flags),
                Err(errno) => reply.error(errno),
            }
        }));
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
        let fs = self.clone();
        drop(self.rt.spawn(async move {
            fs.do_release(fh.0);
            reply.ok();
        }));
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        let fs = self.clone();
        drop(self.rt.spawn(async move {
            match fs.do_readlink(ino.0) {
                Ok(bytes) => reply.data(&bytes),
                Err(errno) => reply.error(errno),
            }
        }));
    }
}
