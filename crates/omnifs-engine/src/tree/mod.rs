//! Renderer-neutral, async-first projection core shared by the omnifs
//! frontends (FUSE, NFS, and the kernel-free itest).
//!
//! `Tree` owns NO kernel state (no inode table, no handle table, no
//! DirSnapshot, no notifier). It wraps the provider registry and re-homes the
//! path-resolution / cache-consult-populate / pagination / invalidation-drain
//! DECISION logic that FUSE and NFS otherwise duplicate. Renderers turn the
//! neutral `Node` / `Listing` / `ReadResult` into their own kernel/protocol
//! identity and reply encoding.
//!
//! # Crate-level hard rules (the "do not reinvent the substrate" contract)
//!
//! - Coalescing stays in `Namespace::coalesced`. `Tree` calls `Namespace` and
//!   gets in-flight dedup for free; `Tree` NEVER adds a second coalescer.
//! - `Tree` calls `Namespace`, NEVER `Runtime::run_op` directly (that would
//!   lose coalescing AND EffectApplier cache-populate).
//! - Byte storage stays in `crate::cache::Store`. `Tree` reaches the cache ONLY
//!   through the mount-owned `Runtime::cache()` handle; it never constructs a
//!   raw store, so the per-mount generation/tombstone fence and mount-prefixed
//!   keys are never bypassed.
//! - wire->cache translation stays in `EffectApplier` (host-internal, inside
//!   `Namespace` methods); `Tree` never imports `wit_types` in its public
//!   surface.
//! - The tree-ref/clone/archive registry stays in `Runtime::resolve_tree_ref`.

// The module docs reference many bare host identifiers, protocol acronyms
// (NFSv4, FUSE), and renderer-side type names (DirSnapshot, FileAttr) as prose,
// not code links; backticking each one harms readability without adding value.
#![allow(clippy::doc_markdown)]

pub mod error;
mod handle;
mod invalidate;
mod list;
mod node;
mod read;
mod resolve;
pub(crate) mod synthetic;

use crate::ServingContext;
use omnifs_api::events::TraceId;

pub use self::error::{RetryClass, TreeError, TreeErrorKind};
pub use self::handle::{RangedHandle, probe_live_growth, spawn_live_follow_pump};
pub use self::invalidate::InvalidationReport;
pub use self::list::{Cursor, ListOutcome, Listing};
pub use self::node::{
    Entry, EntryOrigin, Node, NodeBody, PaginationControl, Synthetic, SyntheticContent,
};
pub use self::read::{Chunk, ReadResult};

/// The renderer-neutral, async-first projection core.
pub struct Tree {
    ctx: ServingContext,
}

/// Per-call observability context, kept optional so the inspector stream
/// survives the extraction (every `Namespace` op already threads
/// `fuse_trace: Option<TraceId>`). A struct (not a bare `Option<TraceId>`) so
/// the later IPC phase can add session/deadline without changing every
/// signature.
#[derive(Debug, Clone, Default)]
pub struct RequestCtx {
    pub trace: Option<TraceId>,
}

impl Tree {
    /// Wrap a [`ServingContext`] (the mount-resolution backing) into the
    /// renderer-neutral projection core.
    pub fn new(ctx: ServingContext) -> Self {
        Self { ctx }
    }

    /// The mount name of the namespace root (see
    /// [`ServingContext::root_mount_name`]).
    pub fn root_mount_name(&self) -> String {
        self.ctx.root_mount_name()
    }

    /// Every mount currently served (see [`ServingContext::served_mounts`]).
    pub fn served_mounts(&self) -> Vec<String> {
        self.ctx.served_mounts()
    }
}
