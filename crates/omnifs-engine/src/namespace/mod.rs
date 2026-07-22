//! The narrow, plain-data namespace surface exposed to frontends.
//!
//! [`Namespace`] is the whole contract a frontend needs to project a mount: name
//! resolution, attributes, directory paging, byte reads, and an invalidation
//! event stream. Every type crossing this boundary is plain data (serde-friendly,
//! no engine internals), so a wire-attached frontend (the only kind) holds a
//! `dyn Namespace` proxy and nothing else of the engine.
//!
//! With the `runtime` feature, the in-engine implementation over the projection
//! tree owns the
//! things a frontend used to re-derive per protocol:
//!
//! - **Identity.** Namespace identities are the validated full protocol paths
//!   from `omnifs_core`. They remain meaningful across daemon replacement, while
//!   frontends keep them opaque and use only namespace answers.
//! - **Policy.** [`Attrs`] carries the already-decided protocol answer: the
//!   sentinel/learned size, the cache TTL, the direct-I/O bit, a change counter,
//!   and a stability class. The frontend copies these into its protocol reply
//!   without re-running FUSE's `ttl_for_attrs` or NFS's change hash.
//! - **Invalidation fan-out.** Every op drains its mount's pending
//!   invalidations before computing its answer (read-your-effects), maps them to
//!   known paths, and emits an event. A background tick keeps events flowing when
//!   no op arrives.
//!
//! # Consistency rule
//!
//! An op stamps every invalidated path into the private change counter used by
//! [`Attrs::change`]. A frontend must not serve protocol state older than the
//! last answer for a path.

use std::path::PathBuf;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use futures::future::BoxFuture;
use omnifs_core::path::Path;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;

use crate::view as view_types;

// -----------------------------------------------------------------------------
// Plain-data surface
// -----------------------------------------------------------------------------

/// Node kind at the namespace boundary. A plain mirror of the projection's kinds
/// so the wire types never depend on `view`/`tree` internals.
///
/// Provider and host-tree nodes use the same ordinary file kinds. Host-tree
/// symlinks are read through [`Namespace::readlink`], never by a frontend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryKind {
    Directory,
    File,
    Symlink,
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
    /// Host identity facts. Provider-derived nodes use zero because the
    /// provider cache has no host filesystem identity.
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub blocks: u64,
    pub mode: u16,
    pub nlink: u32,
    /// Unix epoch milliseconds. This is the compact timestamp representation
    /// already used by the engine cache clock; absent means the source has no
    /// timestamp answer.
    pub accessed: Option<u64>,
    pub modified: Option<u64>,
    pub created: Option<u64>,
    pub ttl: Duration,
    pub change: u64,
    pub direct_io: bool,
    pub stability: StabilityClass,
    /// Whether a frontend materializes the whole payload once or reads through
    /// per request. See [`ReadStyle`].
    pub read_style: ReadStyle,
}

/// The resolved answer for a lookup.
///
/// `path` names the structural child even when it is missing, so protocol
/// caches can invalidate a negative answer without rebuilding projection keys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LookupAnswer {
    pub path: Path,
    pub state: LookupState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LookupState {
    Found { attrs: Attrs },
    Missing { ttl: Duration },
}

impl LookupAnswer {
    #[must_use]
    pub fn found(path: Path, attrs: Attrs) -> Self {
        Self {
            path,
            state: LookupState::Found { attrs },
        }
    }

    #[must_use]
    pub fn missing(path: Path, ttl: Duration) -> Self {
        Self {
            path,
            state: LookupState::Missing { ttl },
        }
    }

    #[must_use]
    pub const fn is_missing(&self) -> bool {
        matches!(self.state, LookupState::Missing { .. })
    }

    #[must_use]
    pub const fn attrs(&self) -> Option<&Attrs> {
        match &self.state {
            LookupState::Found { attrs } => Some(attrs),
            LookupState::Missing { .. } => None,
        }
    }

    #[must_use]
    pub const fn ttl(&self) -> Duration {
        match &self.state {
            LookupState::Found { attrs } => attrs.ttl,
            LookupState::Missing { ttl } => *ttl,
        }
    }
}

/// One directory child.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    pub path: Path,
    pub attrs: Attrs,
}

/// A directory read page: some entries plus an optional continuation cursor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirPage {
    pub entries: Vec<DirEntry>,
    pub next: Option<DirCursor>,
}

/// An opaque directory cursor. `Start` begins a listing; `Provider` continues a
/// provider-paged listing; `Buffered` carries the overflow the per-page `budget`
/// held back, so paging stays stateless (the cursor owns the resume state).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DirCursor {
    Start,
    Provider(view_types::CachedCursor),
    Buffered {
        entries: Vec<DirEntry>,
        then: Option<view_types::CachedCursor>,
        offline: bool,
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

/// A namespace event. Plain data so wire-attached frontends can consume the
/// same stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NsEvent {
    /// The subtree rooted at `path` may have changed; drop protocol-cached state
    /// for it and re-resolve.
    InvalidateSubtree { path: Path },
    /// `node`'s attributes changed in place (a live file grew).
    AttrsChanged { path: Path, attrs: Attrs },
}

impl NsEvent {
    /// Clear all protocol-cached answers after disconnect or event loss.
    #[must_use]
    pub fn reset() -> Self {
        Self::InvalidateSubtree { path: Path::root() }
    }
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
    #[error("durable projection does not contain a complete offline answer")]
    OfflineMiss,
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
            Self::Permission | Self::Invalid | Self::OfflineMiss | Self::Internal { .. } => {
                NsRetryClass::Terminal
            },
        }
    }
}

/// The invalidation event stream a subscriber drives. Wraps a broadcast receiver
/// and converts lag errors into a root invalidation so subscribers resynchronize
/// through the same ordered event channel.
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
        match Pin::new(&mut self.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(event))) => Poll::Ready(Some(event)),
            Poll::Ready(Some(Err(_))) => Poll::Ready(Some(NsEvent::reset())),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
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
    /// Resolve `name` under `parent`, returning the child's structural path.
    fn lookup<'a>(
        &'a self,
        parent: Path,
        name: &'a str,
    ) -> BoxFuture<'a, Result<LookupAnswer, NsError>>;

    /// The current attributes of `path`.
    fn getattr(&self, path: Path) -> BoxFuture<'_, Result<Attrs, NsError>>;

    /// Like [`getattr`](Namespace::getattr), but may perform provider I/O (the
    /// engine's ranged-attr probe) to learn an exact size. The NFS renderer's
    /// directory flattening needs an exact size per child.
    fn getattr_exact(&self, path: Path) -> BoxFuture<'_, Result<Attrs, NsError>>;

    /// Read one directory page from `cursor`, returning at most `budget` entries
    /// (0 = engine default).
    fn readdir(
        &self,
        path: Path,
        cursor: DirCursor,
        budget: usize,
    ) -> BoxFuture<'_, Result<DirPage, NsError>>;

    /// Read `len` bytes at `offset` from `path`.
    fn read(&self, path: Path, offset: u64, len: u32)
    -> BoxFuture<'_, Result<ReadAnswer, NsError>>;

    /// The link target of a symlink node.
    fn readlink(&self, path: Path) -> BoxFuture<'_, Result<PathBuf, NsError>>;

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
