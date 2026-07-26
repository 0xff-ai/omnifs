//! Omnifs VFS facade and optional wire transport.
//!
//! Always available: [`Namespace`] and the plain answer types filesystems consume.
//! With the `wire` feature: postcard framing, handshake, attach/reconnect,
//! readiness, [`WireNamespace`], and [`VfsServer`]. Projection semantics stay
//! in `omnifs-engine`.

mod facade;
pub use facade::*;
pub use omnifs_core::Stability;

#[cfg(feature = "wire")]
mod beacon;
#[cfg(feature = "wire")]
mod client;
#[cfg(feature = "wire")]
mod frame;
#[cfg(feature = "wire")]
mod server;
#[cfg(all(feature = "wire", test))]
mod tests;

#[cfg(feature = "wire")]
use std::path::PathBuf;

#[cfg(feature = "wire")]
use omnifs_core::path::Path;
#[cfg(feature = "wire")]
use serde::{Deserialize, Serialize};

#[cfg(target_os = "linux")]
#[cfg(feature = "wire")]
pub use beacon::spawn_ready_signal;
#[cfg(feature = "wire")]
pub use beacon::{ReadyPortError, resolve_ready_vsock_port};
#[cfg(feature = "wire")]
pub use client::{
    AttachTarget, AttachTargetError, TeardownOutcome, TeardownReason, TeardownRequest,
    WireNamespace,
};
#[cfg(feature = "wire")]
pub use server::{Endpoint, ListenerEvent, VfsServer, serve_connection};

/// The Omnifs VFS wire protocol version. Peers that disagree refuse to serve.
#[cfg(feature = "wire")]
pub const PROTOCOL: u32 = 9;

#[cfg(feature = "wire")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum ServerControl {
    Stop,
}

#[cfg(feature = "wire")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum WireRequest {
    Lookup {
        parent: Path,
        name: String,
    },
    Getattr {
        path: Path,
    },
    GetattrExact {
        path: Path,
    },
    Readdir {
        path: Path,
        cursor: DirCursor,
        budget: u64,
    },
    Read {
        path: Path,
        offset: u64,
        len: u32,
    },
    Readlink {
        path: Path,
    },
}

#[cfg(feature = "wire")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum WireResponse {
    Lookup(Result<LookupAnswer, NsError>),
    Getattr(Result<Attrs, NsError>),
    GetattrExact(Result<Attrs, NsError>),
    Readdir(Result<DirPage, NsError>),
    Read(Result<ReadAnswer, NsError>),
    Readlink(Result<PathBuf, NsError>),
}

#[cfg(feature = "wire")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum Handshake {
    Hello {
        protocol: u32,
        filesystem: omnifs_core::fs::Spec,
    },
    Welcome {
        protocol: u32,
    },
    Rejected {
        reason: String,
    },
}

#[cfg(feature = "wire")]
#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("wire io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("wire protocol error: {0}")]
    Protocol(String),
    #[error("frame len {len} exceeds the 16 MiB maximum")]
    FrameTooLarge { len: u32 },
    #[error("wire encoding error: {0}")]
    Encoding(#[from] postcard::Error),
    #[error("protocol version mismatch: this build speaks {ours}, the peer speaks {theirs}")]
    VersionMismatch { ours: u32, theirs: u32 },
    #[error("connection closed during the handshake")]
    HandshakeClosed,
    #[error("expected a {expected} handshake frame")]
    HandshakeUnexpected { expected: &'static str },
    #[error("attach rejected by the daemon: {0}")]
    Rejected(String),
    #[error(
        "could not reach the namespace attach target {target} within the connect deadline: {source}"
    )]
    ConnectTimeout {
        target: String,
        source: std::io::Error,
    },
    #[error("vsock attach is not supported on this platform")]
    VsockUnsupported,
}
