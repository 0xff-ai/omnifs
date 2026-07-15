//! Renderer-neutral, async-first projection core shared by the omnifs
//! frontends (FUSE, NFS, and the kernel-free itest).
//!
//! The namespace projection owns NO kernel state (no inode table, no handle table, no
//! DirSnapshot, no notifier). It wraps the provider registry and re-homes the
//! path-resolution, cache, pagination, and invalidation policy that FUSE and NFS
//! would otherwise duplicate. Renderers turn the
//! neutral `Node` / `Listing` / `ReadResult` into their own kernel/protocol
//! identity and reply encoding.
//!
//! # Crate-level hard rules (the "do not reinvent the substrate" contract)
//!
//! - Coalescing stays in the runtime's typed flights. The projection never adds
//!   a second coalescer.
//! - The projection calls typed runtime operations, NEVER `Runtime::run_op` directly (that would
//!   lose coalescing AND EffectApplier cache-populate).
//! - Byte storage stays in the mount-owned `MountResources`. The projection reaches
//!   cache state only through the running mount's resource owner; it never constructs a
//!   raw store, so the per-mount generation/tombstone fence and mount-prefixed
//!   keys are never bypassed.
//! - wire->cache translation stays in `EffectApplier` (host-internal, inside
//!   runtime methods); the public namespace never imports `wit_types`.
//! - The tree-ref/clone registry stays in the runtime's private WIT handle table.

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

pub(crate) use self::error::{TreeError, TreeErrorKind};
pub(crate) use self::handle::{RangedHandle, spawn_live_follow_pump};
pub(crate) use self::list::{Cursor, ListOutcome};
pub(crate) use self::node::HostKind;
pub(crate) use self::node::{Node, NodeBody};
pub(crate) use self::read::ReadResult;
/// Per-call request policy context. Observability follows the active tracing
/// span rather than being threaded through tree operations.
#[derive(Debug, Clone, Default)]
pub struct RequestCtx;
