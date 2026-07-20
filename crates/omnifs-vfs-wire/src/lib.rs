//! The Omnifs VFS wire protocol.
//!
//! [`Namespace`](omnifs_engine::Namespace) in `omnifs-engine` owns shared VFS
//! semantics. This crate owns only their transport representation: postcard
//! serialization, length-delimited framing, handshake, attach target resolution
//! and reconnect, readiness signaling, and ordered namespace events.
//!
//! The daemon serves a [`TreeNamespace`](omnifs_engine::TreeNamespace) over a
//! [`VfsServer`]; an out-of-process renderer attaches a
//! [`WireNamespace`] and holds a `dyn Namespace` that speaks frames instead of
//! calling the engine directly.
//!
//! There is no RPC framework. The wire is the length-delimited [`frame`] codec
//! plus a fixed handshake: one request or response is one postcard-encoded
//! frame, multiplexed by an id.
//!
//! # Handshake
//!
//! On connect the client sends one `Hello { protocol, frontend }`
//! request frame (`request_id = 0`), naming itself with a [`FrontendIdentity`]
//! so the server can track it live. The server replies
//! with either `Welcome { protocol }` or `Rejected { reason }`
//! (both response frames, `request_id = 0`), then closes the connection in the
//! rejected case. Auth is transport-local: UDS uses filesystem mode; TCP binds
//! only loopback or a verified docker0 address; vsock uses the host proxy path.
//! A protocol mismatch is rejected with a named reason. Reconnect identity is
//! carried by the ordered namespace event stream, not by a second attach channel.
//!
//! `frontend` is display-only for the host: the guest names its own kind and
//! mount point so the daemon's status surface can report it, but the host
//! decides how the connection was *delivered* (native/docker/libkrun/external)
//! from which listener it arrived on, never from anything the guest claims.
//! [`VfsServer`] owns that listener authority and removes an observed identity
//! when its last connection ends.
//!
//! # Identity
//!
//! Steady-state request ids start at 1 and increase per request; a response
//! carries the id of the request it answers, so the client matches replies to
//! callers even when the server answers out of order. Events carry
//! `request_id = 0` and `kind = KIND_EVENT`.

mod beacon;
mod client;
mod frame;
mod server;
#[cfg(test)]
mod tests;

use std::path::PathBuf;

use omnifs_core::path::Path;
use omnifs_engine::{Attrs, DirCursor, LookupAnswer, NsError, ReadAnswer};
use serde::{Deserialize, Serialize};

#[cfg(target_os = "linux")]
pub use beacon::spawn_ready_signal;
pub use beacon::{ReadyPortError, resolve_ready_vsock_port};
pub use client::{AttachTarget, AttachTargetError, WireNamespace};
pub use server::{ListenerEvent, ListenerTarget, VfsServer, serve_connection};

/// The Omnifs VFS wire protocol version. A client and server that disagree refuse
/// to serve: there is no version negotiation, so v6 rejects a v5 (or lower)
/// peer outright with a named reason.
pub const PROTOCOL: u32 = 7;

/// Identity a connecting frontend presents in its handshake `Hello`, naming
/// its own kind and guest-side mount point (display-only). The server reports
/// it. The host derives delivery (local/docker/libkrun)
/// from the listener that accepted the connection, never from the guest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontendIdentity {
    pub kind: FrontendKind,
    /// The guest-side mount point this frontend serves. Display-only; the
    /// host does not treat it as host-visible.
    pub mount_point: PathBuf,
}

/// The protocol kind a frontend renders (FUSE or NFS). Unrelated to
/// [`omnifs_api::FsType`], which is the OS-visible filesystem type reported
/// for a live attachment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FrontendKind {
    Fuse,
    Nfs,
}

/// One namespace call, mirroring the [`Namespace`](omnifs_engine::Namespace)
/// trait methods. `budget` is a `u64` on the wire (the trait takes `usize`); the
/// server narrows it back per platform.
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

/// One namespace answer. Each variant carries the whole `Result` its method
/// returns, so a server-side [`NsError`] is postcard-encoded and re-raised on the
/// client verbatim. The variant selects which method the answer is for; the
/// client matches it against the request it multiplexed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum WireResponse {
    Lookup(Result<LookupAnswer, NsError>),
    Getattr(Result<Attrs, NsError>),
    GetattrExact(Result<Attrs, NsError>),
    Readdir(Result<omnifs_engine::DirPage, NsError>),
    Read(Result<ReadAnswer, NsError>),
    Readlink(Result<PathBuf, NsError>),
}

/// The handshake payloads, carried in the `request_id = 0` frames each side
/// sends first. The frame `kind` (request vs response) already distinguishes the
/// direction; the enum keeps a wrong-direction message detectable.
///
/// `frontend` names the connecting frontend so [`VfsServer`] can report it in
/// its live snapshot; display-only, never used for a trust decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum Handshake {
    Hello {
        protocol: u32,
        frontend: FrontendIdentity,
    },
    Welcome {
        protocol: u32,
    },
    /// The server refused the handshake (typically a protocol mismatch) and is
    /// about to close the connection. Sent so the client gets a terminal, named
    /// reason instead of an ambiguous closed pipe.
    Rejected {
        reason: String,
    },
}

/// A wire fault. Frame-level faults (a short read, an oversized `len`, a
/// malformed payload) drop the connection; the client's per-request callers see
/// [`NsError::Network`](omnifs_engine::NsError::Network) instead.
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
    /// The server sent [`Handshake::Rejected`] naming why (typically a version
    /// mismatch). Not retriable without changing the peer or the build.
    #[error("attach rejected by the daemon: {0}")]
    Rejected(String),
    #[error(
        "could not reach the namespace attach target {target} within the connect deadline: {source}"
    )]
    ConnectTimeout {
        target: String,
        source: std::io::Error,
    },
    /// A [`crate::AttachTarget::Vsock`] attach was attempted on a build that
    /// cannot dial vsock (only the Linux libkrun guest can). Not retriable:
    /// the platform will not change mid-run.
    #[error("vsock attach is not supported on this platform")]
    VsockUnsupported,
}
