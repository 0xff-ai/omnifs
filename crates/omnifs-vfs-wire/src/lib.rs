//! The Omnifs VFS wire protocol.
//!
//! [`Namespace`](omnifs_engine::Namespace) in `omnifs-engine` owns shared VFS
//! semantics. This crate owns only their transport representation: postcard
//! serialization, length-delimited framing, handshake, attach target resolution
//! and reconnect, readiness signaling, and the client wire cache.
//!
//! The daemon serves a [`TreeNamespace`](omnifs_engine::TreeNamespace) over a
//! byte stream with [`serve_listener`]; an out-of-process renderer attaches a
//! [`WireNamespace`] and holds a `dyn Namespace` that speaks frames instead of
//! calling the engine directly.
//!
//! There is no RPC framework. The wire is the length-delimited [`frame`] codec
//! plus a fixed handshake: one request or response is one postcard-encoded
//! frame, multiplexed by an id.
//!
//! # Handshake
//!
//! On connect the client sends one `Hello { protocol, token, frontend }`
//! request frame (`request_id = 0`), naming itself with a [`FrontendIdentity`]
//! so the daemon's frontend registry can track it live. The server replies
//! with either `Welcome { protocol, instance_id }` or `Rejected { reason }`
//! (both response frames, `request_id = 0`), then closes the connection in the
//! rejected case. A plain UDS listener ignores `token` (filesystem permissions
//! are that transport's whole auth); a TCP attach listener, and a UDS listener
//! bound with a token (the krunkit vsock-proxy path, where every guest dial
//! looks like the same trusted local peer to the socket), both require it to
//! match the per-instance attach token. A protocol mismatch is rejected the
//! same way. `instance_id` is the daemon's per-start id: a reconnect that
//! lands on a different id means the daemon restarted and every [`NodeId`] the
//! client holds is stale.
//!
//! `frontend` is display-only for the host: the guest names its own kind and
//! mount point so the daemon's status surface can report it, but the host
//! decides how the connection was *delivered* (native/docker/krunkit/external)
//! from which listener it arrived on, never from anything the guest claims. A
//! server that tracks attach lifecycle passes an [`AttachObserver`] into
//! [`serve_connection`]/[`serve_listener`]/[`serve_listener_tcp`]; its
//! `attached` fires once per successful handshake and `detached` fires when
//! that connection ends, for any reason.
//!
//! # Identity
//!
//! Steady-state request ids start at 1 and increase per request; a response
//! carries the id of the request it answers, so the client matches replies to
//! callers even when the server answers out of order. Events carry
//! `request_id = 0` and `kind = KIND_EVENT`.

mod beacon;
mod cache;
mod client;
mod frame;
mod server;
#[cfg(test)]
mod tests;

use std::path::PathBuf;

use omnifs_engine::{Attrs, DirCursor, NodeAnswer, NodeId, NsError, ReadAnswer};
use serde::{Deserialize, Serialize};

#[cfg(target_os = "linux")]
pub use beacon::spawn_ready_signal;
pub use beacon::{ReadyPortError, resolve_ready_vsock_port};
pub use client::{AttachTarget, AttachTargetError, WireNamespace};
pub use server::{serve_connection, serve_listener, serve_listener_tcp};

/// The Omnifs VFS wire protocol version. Bumped on any incompatible change to
/// the frame payloads or handshake. A client and server that disagree refuse
/// to serve: there is no version negotiation, so v3 rejects a v2 (or lower)
/// peer outright. Bumped 2 to 3 to carry [`FrontendIdentity`] in `Hello`.
pub const PROTOCOL: u32 = 3;

/// Identity a virtualized frontend presents in its handshake `Hello`, naming
/// its own kind and guest-side mount point so the daemon's frontend registry
/// can report it. Display-only for the host: the host owns trust and derives
/// the *delivery* mechanism (native/docker/krunkit/external) from which
/// listener the connection arrived on, never from anything the guest claims
/// here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontendIdentity {
    pub kind: FrontendKind,
    /// The guest-side mount point this frontend serves. Display-only; the
    /// host does not treat it as host-visible.
    pub mount_point: PathBuf,
}

/// The protocol a virtualized frontend renders over its wire-attached
/// namespace. Mirrors the two shipped renderers; unrelated to
/// [`omnifs_api::FsType`], which describes a native, in-process frontend's OS
/// mount table entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FrontendKind {
    Fuse,
    Nfs,
}

/// Observes the attach lifecycle of connections on a wire listener. A server
/// that wants to track which virtualized frontends are currently attached
/// (the daemon's frontend registry) passes one in; a bare test server passes
/// `None`.
pub trait AttachObserver: Send + Sync {
    /// Called once, right after a connection completes its handshake.
    /// Returns an opaque id that [`Self::detached`] receives back when that
    /// same connection ends.
    fn attached(&self, identity: &FrontendIdentity) -> u64;
    /// Called when the connection that produced `id` ends, for any reason: an
    /// orderly disconnect, a protocol fault, or a panic in the serve loop.
    /// Fired from a drop guard so it cannot be skipped.
    fn detached(&self, id: u64);
}

/// One namespace call, mirroring the [`Namespace`](omnifs_engine::Namespace)
/// trait methods. `budget` is a `u64` on the wire (the trait takes `usize`); the
/// server narrows it back per platform.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum WireRequest {
    Lookup {
        parent: NodeId,
        name: String,
    },
    Getattr {
        node: NodeId,
    },
    GetattrExact {
        node: NodeId,
    },
    Readdir {
        node: NodeId,
        cursor: DirCursor,
        budget: u64,
    },
    Read {
        node: NodeId,
        offset: u64,
        len: u32,
    },
    Readlink {
        node: NodeId,
    },
}

/// One namespace answer. Each variant carries the whole `Result` its method
/// returns, so a server-side [`NsError`] is postcard-encoded and re-raised on the
/// client verbatim. The variant selects which method the answer is for; the
/// client matches it against the request it multiplexed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum WireResponse {
    Lookup(Result<NodeAnswer, NsError>),
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
/// `token` is `None` over a Unix socket (the client has nothing to prove
/// beyond the filesystem permissions that let it open the socket); a TCP
/// attach listener requires it and rejects a mismatch via `Rejected`.
/// `frontend` names the connecting frontend so the server-side
/// [`AttachObserver`] (when present) can report it; display-only, never used
/// for a trust decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum Handshake {
    Hello {
        protocol: u32,
        token: Option<String>,
        frontend: FrontendIdentity,
    },
    Welcome {
        protocol: u32,
        instance_id: String,
    },
    /// The server refused the handshake (a protocol mismatch or a bad attach
    /// token) and is about to close the connection. Sent so the client gets a
    /// terminal, named reason instead of an ambiguous closed pipe.
    Rejected {
        reason: String,
    },
}

/// A change in the server the client is attached to. Fires only when a reconnect
/// lands on a *different* daemon instance than before; a plain reconnect to the
/// same instance fires nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachEvent {
    /// The daemon restarted under the client: every [`NodeId`] the consumer holds
    /// is invalid. The out-of-process NFS test runner translates this into a
    /// namespace reattach event; FUSE records it for observability.
    Reattached {
        old_instance: String,
        new_instance: String,
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
    /// The TCP attach listener's token did not match. Not retriable: unlike a
    /// refused or dropped connection, presenting the same token again cannot
    /// succeed.
    #[error("attach token rejected")]
    TokenRejected,
    /// The server sent [`Handshake::Rejected`] naming why (a version mismatch
    /// or a bad token). Not retriable for the same reason as
    /// [`WireError::TokenRejected`].
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
    /// cannot dial vsock (only the Linux krunkit guest can). Not retriable:
    /// the platform will not change mid-run.
    #[error("vsock attach is not supported on this platform")]
    VsockUnsupported,
}
