//! The narrow, plain-data namespace surface exposed to frontends.
//!
//! [`Namespace`] is the whole contract a frontend needs to project a mount: name
//! resolution, attributes, directory paging, byte reads, and an invalidation
//! event stream. Every type crossing this boundary is plain data (serde-friendly,
//! no engine internals), so in-process and wire-attached frontends both hold a
//! `dyn Namespace` and nothing else.
//!
//! With the `runtime` feature, the in-engine implementation over the projection
//! tree owns the
//! things a frontend used to re-derive per protocol:
//!
//! - **Identity.** A [`NodeId`] is an opaque, engine-owned handle. The engine
//!   table maps it to the (mount, mount-relative path) the projection speaks;
//!   `NodeId(1)` is the namespace root. Ids are NOT stable across a daemon
//!   restart; within a session an id keeps its meaning so a frontend can cache
//!   it. Consumers must not reuse ids after observing a daemon restart.
//! - **Policy.** [`Attrs`] carries the already-decided protocol answer: the
//!   sentinel/learned size, the cache TTL, the direct-I/O bit, a change counter,
//!   and a stability class. The frontend copies these into its protocol reply
//!   without re-running FUSE's `ttl_for_attrs` or NFS's change hash.
//! - **Invalidation fan-out.** Every op drains its mount's pending
//!   invalidations before computing its answer (read-your-effects), maps them to
//!   the ids this table knows, bumps an epoch, and emits an event. A background
//!   tick keeps events flowing when no op arrives.
//!
//! # Consistency rule
//!
//! An op stamps every id it invalidates with the current epoch; the id's next
//! answer carries that epoch through [`Attrs::change`]. A frontend must not serve
//! protocol state older than the epoch of a node's last answer.

use std::path::PathBuf;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;

use crate::view as view_types;
/// The reserved namespace-root id.
const ROOT_ID: u64 = 1;

// -----------------------------------------------------------------------------
// Plain-data surface
// -----------------------------------------------------------------------------

/// Opaque, engine-owned node handle. The engine maps it to a (mount, path); a
/// frontend treats it as a token and never inspects the integer.
///
/// No cross-restart persistence: a daemon restart may renumber ids, so consumers
/// must discard ids associated with the old instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct NodeId(pub u64);

impl NodeId {
    /// The namespace root: the mount-enumeration directory (or the single/rooted
    /// mount's root). Every resolution starts here.
    pub const ROOT: NodeId = NodeId(ROOT_ID);
}

/// A monotonic invalidation epoch. Bumped once per non-empty invalidation report;
/// stamped onto the nodes that report touched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Epoch(pub u64);

/// Node kind at the namespace boundary. A plain mirror of the projection's kinds
/// so the wire types never depend on `view`/`tree` internals.
///
/// `Symlink` is reserved: the projection does not produce symlinks, but the
/// variant keeps the wire shape complete.
/// `Subtree` is a local-directory handoff (a resolved treeref clone/archive).
/// The consumer can serve `root` only when that path is accessible in its
/// filesystem namespace; virtualized frontends cannot dereference a host-local
/// path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryKind {
    Directory,
    File,
    Symlink,
    Subtree { root: PathBuf },
}

/// Freshness class of a file, plain-data mirror of `view::Stability`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StabilityClass {
    Stable,
    Dynamic,
    Live,
}

/// How a frontend pulls a file's bytes, decided by the engine.
///
/// - `Whole`: the engine serves the entire payload from a single
///   `read(node, 0, u32::MAX)`. A frontend materializes it once per open and
///   slices locally, so a mutating control action (`@next`) or an unversioned
///   dynamic render runs exactly once. Every non-ranged file (inline, canonical,
///   blob, deferred-full) and every directory is `Whole`.
/// - `Ranged`: the engine streams by `read(node, offset, len)` per request,
///   deduping the provider open behind its internal handle cache and learning
///   the exact size on an EOF-short read. A frontend reads through per protocol
///   read rather than buffering, so a live (`tail -f`) or large ranged file is
///   never fully materialized.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReadStyle {
    Whole,
    Ranged,
}

/// The protocol attributes for a node. The engine bakes shared policy into this
/// answer:
///
/// - `size` is the protocol size after the sentinel and learned-size rules (an
///   unknown-length file reports `1`, a completed read promotes the exact size),
/// - `ttl` is the engine-decided protocol cache lifetime,
/// - `change` is a version counter for the NFS change attribute, stamped with the
///   node's last invalidation epoch,
/// - `direct_io` carries the engine's direct-I/O decision,
/// - `stability` classifies the file's freshness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attrs {
    pub kind: EntryKind,
    pub size: u64,
    pub ttl: Duration,
    pub change: u64,
    pub direct_io: bool,
    pub stability: StabilityClass,
    /// Whether a frontend materializes the whole payload once or reads through
    /// per request. See [`ReadStyle`].
    pub read_style: ReadStyle,
}

/// The resolved answer for a lookup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeAnswer {
    pub node: NodeId,
    pub attrs: Attrs,
    pub kind: EntryKind,
}

/// One directory child.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    pub node: NodeId,
    pub attrs: Attrs,
    pub kind: EntryKind,
}

/// A directory read page: some entries plus an optional continuation cursor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirPage {
    pub entries: Vec<DirEntry>,
    pub next: Option<DirCursor>,
}

/// An opaque directory cursor. `Start` begins a listing; `Tree` continues a
/// provider-paged listing; `Buffered` carries the overflow the per-page `budget`
/// held back, so paging stays stateless (the cursor owns the resume state).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DirCursor {
    Start,
    Tree(view_types::CachedCursor),
    Buffered {
        entries: Vec<DirEntry>,
        then: Option<view_types::CachedCursor>,
    },
}

impl DirCursor {
    /// Begin a directory listing.
    pub fn start() -> Self {
        Self::Start
    }
}

/// The answer for one byte read. `attrs` lets a caller promote a learned size
/// without a second `getattr`: the learned-size writeback that FUSE/NFS did per
/// protocol is engine-internal here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadAnswer {
    pub bytes: Vec<u8>,
    pub eof: bool,
    pub attrs: Attrs,
}

/// A namespace event. Plain data so in-process and wire-attached frontends can
/// consume the same stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NsEvent {
    /// The subtree rooted at `node` may have changed; drop protocol-cached state
    /// for it and re-resolve.
    InvalidateSubtree { node: NodeId, epoch: Epoch },
    /// `node`'s attributes changed in place (a live file grew).
    AttrsChanged {
        node: NodeId,
        attrs: Attrs,
        epoch: Epoch,
    },
}

/// A change in the daemon a frontend is attached to, delivered out of band from
/// the [`NsEvent`] invalidation stream. It fires when an out-of-process
/// renderer's wire connection reconnects onto a *restarted* daemon: every
/// [`NodeId`] the renderer cached is meaningless against the new instance and
/// must be re-resolved. The out-of-process runner bridges its wire attach events
/// into this engine-owned type so a frontend crate need not depend on the wire
/// crate to act on a reattach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NsAttachEvent {
    /// The daemon restarted under the renderer; drop every cached `NodeId` and
    /// re-resolve lazily from the surviving protocol identity chain.
    Reattached,
}

/// Retry classification for an [`NsError`], derivable without importing the
/// engine's tree errors. Mirrors the frontend `retry_class` partition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NsRetryClass {
    Retry,
    Gone,
    Terminal,
    TooLarge,
}

/// Plain-data error surface. Mirrors the frontend-relevant classification of the
/// engine's tree errors plus the retry class, so a frontend maps to errno /
/// nfsstat4 without importing engine internals.
///
/// Serde-derived because it crosses the Omnifs VFS wire protocol inside every
/// `WireResponse` (`omnifs-vfs-wire`): a server-side op failure is
/// postcard-encoded and re-raised on the client renderer verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum NsError {
    #[error("not found")]
    NotFound,
    #[error("not a directory")]
    NotDirectory,
    #[error("is a directory")]
    IsDirectory,
    #[error("permission denied")]
    Permission,
    #[error("invalid argument")]
    Invalid,
    #[error("too large")]
    TooLarge,
    #[error("rate limited")]
    RateLimited { retry_after: Option<Duration> },
    #[error("timeout")]
    Timeout,
    #[error("network error")]
    Network,
    #[error("internal error: {message}")]
    Internal { message: String },
}

impl NsError {
    /// How a frontend should treat this error on retry.
    pub fn retry_class(&self) -> NsRetryClass {
        match self {
            Self::RateLimited { .. } | Self::Timeout | Self::Network => NsRetryClass::Retry,
            Self::NotFound | Self::NotDirectory | Self::IsDirectory => NsRetryClass::Gone,
            Self::TooLarge => NsRetryClass::TooLarge,
            Self::Permission | Self::Invalid | Self::Internal { .. } => NsRetryClass::Terminal,
        }
    }
}

/// The invalidation event stream a subscriber drives. Wraps a broadcast receiver
/// and drops lag errors (a lagged subscriber simply resyncs on the next event).
pub struct EventStream {
    inner: BroadcastStream<NsEvent>,
}

impl EventStream {
    /// Build an event stream over an arbitrary broadcast receiver.
    ///
    /// The in-engine runtime implementation taps its own broadcast sender, but a
    /// VFS wire client (`omnifs-vfs-wire`) re-broadcasts the events it
    /// decodes off the socket through a local channel and hands frontends an
    /// `EventStream` over that receiver; this is the constructor it needs.
    #[must_use]
    pub fn from_broadcast(receiver: broadcast::Receiver<NsEvent>) -> Self {
        Self {
            inner: BroadcastStream::new(receiver),
        }
    }

    /// Await the next event, or `None` when the sender is gone.
    pub async fn recv(&mut self) -> Option<NsEvent> {
        use futures::StreamExt;
        self.next().await
    }

    /// Non-blocking drain of one buffered event, `None` when none is ready.
    ///
    /// The NFS frontend's `ReadOnlyExport` methods are synchronous and must
    /// apply invalidation events with drain-before-answer ordering (a stat that
    /// sees its own invalidation must prune before it re-reads its inode). A
    /// detached subscriber task cannot guarantee that ordering against a
    /// synchronous caller, so the frontend polls the buffered events inline
    /// after each namespace op emits them. A lagged receiver skips the gap; the
    /// next answer re-resolves fresh state regardless.
    pub fn try_recv(&mut self) -> Option<NsEvent> {
        use futures::Stream;
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        match Stream::poll_next(Pin::new(self), &mut cx) {
            Poll::Ready(event) => event,
            Poll::Pending => None,
        }
    }
}

impl futures::Stream for EventStream {
    type Item = NsEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<NsEvent>> {
        loop {
            match Pin::new(&mut self.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(event))) => return Poll::Ready(Some(event)),
                Poll::Ready(Some(Err(_))) => {},
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

// -----------------------------------------------------------------------------
// The trait
// -----------------------------------------------------------------------------

/// The narrow namespace surface a frontend consumes. Dyn-compatible: methods
/// return [`BoxFuture`] rather than `async fn`, so the projection has no
/// async-trait dependency and a frontend can hold a `dyn Namespace`.
pub trait Namespace: Send + Sync {
    /// Resolve `name` under `parent`, allocating a stable id for the child.
    fn lookup<'a>(
        &'a self,
        parent: NodeId,
        name: &'a str,
    ) -> BoxFuture<'a, Result<NodeAnswer, NsError>>;

    /// The current attributes of `node`.
    fn getattr(&self, node: NodeId) -> BoxFuture<'_, Result<Attrs, NsError>>;

    /// Like [`getattr`](Namespace::getattr), but may perform provider I/O (the
    /// engine's ranged-attr probe) to learn an exact size. The NFS renderer's
    /// directory flattening needs an exact size per child.
    fn getattr_exact(&self, node: NodeId) -> BoxFuture<'_, Result<Attrs, NsError>>;

    /// Read one directory page from `cursor`, returning at most `budget` entries
    /// (0 = engine default).
    fn readdir(
        &self,
        node: NodeId,
        cursor: DirCursor,
        budget: usize,
    ) -> BoxFuture<'_, Result<DirPage, NsError>>;

    /// Read `len` bytes at `offset` from `node`.
    fn read(
        &self,
        node: NodeId,
        offset: u64,
        len: u32,
    ) -> BoxFuture<'_, Result<ReadAnswer, NsError>>;

    /// The link target of a symlink node.
    fn readlink(&self, node: NodeId) -> BoxFuture<'_, Result<PathBuf, NsError>>;

    /// Subscribe to invalidation events.
    fn subscribe(&self) -> EventStream;
}

// -----------------------------------------------------------------------------
// TreeNamespace
// -----------------------------------------------------------------------------

#[cfg(feature = "runtime")]
mod implementation;

#[cfg(feature = "runtime")]
pub use implementation::TreeNamespace;
