//! The namespace surface over a byte stream.
//!
//! This crate serializes the phase-2 [`Namespace`](omnifs_engine::Namespace)
//! trait so a frontend renderer can run out of process from the daemon that owns
//! the projection. The daemon serves a [`TreeNamespace`](omnifs_engine::TreeNamespace)
//! over a Unix socket with [`serve_listener`]; a renderer attaches a
//! [`WireNamespace`] to that socket and holds a `dyn Namespace` that speaks
//! frames instead of calling the engine directly.
//!
//! There is no RPC framework here by design (ratified decision D4). The wire is
//! the length-delimited [`frame`] codec plus a fixed handshake: one request or
//! response is one postcard-encoded frame, multiplexed by an id.
//!
//! # Handshake
//!
//! On connect the client sends one `Hello { protocol }` request frame
//! (`request_id = 0`) and the server replies with one `Welcome { protocol,
//! instance_id }` response frame (`request_id = 0`). A protocol mismatch is a
//! clean error that closes the connection. `instance_id` is the daemon's
//! per-start id: a reconnect that lands on a different id means the daemon
//! restarted and every [`NodeId`] the client holds is stale.
//!
//! # Identity
//!
//! Steady-state request ids start at 1 and increase per request; a response
//! carries the id of the request it answers, so the client matches replies to
//! callers even when the server answers out of order. Events carry
//! `request_id = 0` and `kind = KIND_EVENT`.

mod cache;
mod client;
mod frame;
mod server;
#[cfg(test)]
mod tests;

use std::path::PathBuf;

use omnifs_engine::{Attrs, DirCursor, NodeAnswer, NodeId, NsError, ReadAnswer};
use serde::{Deserialize, Serialize};

pub use client::WireNamespace;
pub use server::{serve_connection, serve_listener};

/// The wire protocol version. Bumped on any incompatible change to the frame
/// payloads or the handshake. A client and server that disagree refuse to serve.
pub const PROTOCOL: u32 = 1;

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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum Handshake {
    Hello { protocol: u32 },
    Welcome { protocol: u32, instance_id: String },
}

/// A change in the server the client is attached to. Fires only when a reconnect
/// lands on a *different* daemon instance than before; a plain reconnect to the
/// same instance fires nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachEvent {
    /// The daemon restarted under the client: every [`NodeId`] the consumer holds
    /// is invalid and must be re-resolved from the root. Part B teaches the NFS
    /// renderer to act on this; today it is exposed for observation.
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
    #[error("could not reach the namespace socket {socket} within the connect deadline: {source}")]
    ConnectTimeout {
        socket: PathBuf,
        source: std::io::Error,
    },
}
