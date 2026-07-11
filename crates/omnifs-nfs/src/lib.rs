//! Read-only NFSv4.0 loopback frontend for `omnifs`.
//!
//! This crate owns the NFS frontend and adapts it to the existing
//! `omnifs-engine` provider runtime without replacing the Linux FUSE path.
//! The `Export` adapter intentionally depends on host runtime/cache
//! types; the Omnifs VFS wire protocol remains isolated behind `ReadOnlyExport`.

mod adapter;
mod delayed;
mod error;
mod export;
mod mount;
mod persist;
mod protocol;
mod server;
mod trace;

pub use adapter::Export;
pub use error::NfsFrontendError;
pub use export::{
    Attr, DirEntry, DirListing, NodeKind, OpenRead, OpenResult, ReadOnlyExport, StateId, Status,
    StatusResult,
};
pub use mount::{
    NfsMountOptions, mount_blocking, mount_is_active, mount_is_active_checked, mount_is_omnifs,
    unmount,
};
pub use protocol::consts::{
    NFS4_OK, NFS4ERR_ACCESS, NFS4ERR_INVAL, NFS4ERR_IO, NFS4ERR_ISDIR, NFS4ERR_NOENT,
    NFS4ERR_NOTDIR, NFS4ERR_RESOURCE, NFS4ERR_ROFS, NFS4ERR_STALE,
};
pub use server::{RunningNfsServer, start_server};
