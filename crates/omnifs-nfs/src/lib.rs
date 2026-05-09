//! Read-only NFSv4.0 loopback frontend for `OmnIFS`.
//!
//! This crate owns the NFS frontend and adapts it to the existing
//! `omnifs-host` provider runtime without replacing the Linux FUSE path.
//! The `OmnifsExport` adapter intentionally depends on host runtime/cache
//! types; the wire protocol code remains isolated behind `ReadOnlyExport`.

mod adapter;
mod error;
mod export;
mod mount;
mod protocol;
mod server;
mod trace;

pub use adapter::OmnifsExport;
pub use error::NfsFrontendError;
pub use export::{NfsAttr, NfsDirEntry, NfsNodeKind, NfsResult, ReadOnlyExport};
pub use mount::{NfsMountOptions, mount_blocking, unmount};
pub use protocol::consts::{
    NFS4_OK, NFS4ERR_ACCESS, NFS4ERR_INVAL, NFS4ERR_IO, NFS4ERR_ISDIR, NFS4ERR_NOENT,
    NFS4ERR_NOTDIR, NFS4ERR_RESOURCE, NFS4ERR_ROFS, NFS4ERR_STALE,
};
pub use server::{RunningNfsServer, start_server};
