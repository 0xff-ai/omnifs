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
//!   lose coalescing AND Materializer cache-populate).
//! - Byte storage stays in `omnifs_cache::Store`. `Tree` reaches the cache ONLY
//!   through `Runtime` pass-throughs (cache_get/current_generation/
//!   write_fenced/canonical_bytes_for/...); it never holds a raw `Store`
//!   handle, so the per-mount generation/tombstone fence and mount-prefixed
//!   keys are never bypassed.
//! - wire->cache translation stays in `Materializer` (host-internal, inside
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
mod synthetic;

use std::sync::Arc;

use omnifs_core::path::Path;
use omnifs_host::Runtime;
use omnifs_host::registry::ProviderRegistry;
use omnifs_inspector::TraceId;

pub use crate::error::{Result, TreeError, TreeErrorKind};
pub use crate::handle::{RangedHandle, probe_live_growth};
pub use crate::invalidate::{InvalidationReport, WatchStream};
pub use crate::list::{Cursor, ListOutcome, Listing};
pub use crate::node::{
    Entry, EntryOrigin, Node, NodeBody, NodeId, PaginationControl, Synthetic, SyntheticContent,
};
pub use crate::read::{Chunk, ReadResult};

/// Internal mount-resolution backing. `Tree::new` wraps a full
/// `ProviderRegistry` (the production form both renderers hold). `for_runtime`
/// wraps a single bare `Arc<Runtime>` under a fixed mount name (the itest /
/// single-mount embedding form), because `ProviderRegistry::add_mount`
/// instantiates wasm itself and cannot be populated from a bare `Runtime`.
enum Mounts {
    Registry(Arc<ProviderRegistry>),
    Single {
        mount: String,
        runtime: Arc<Runtime>,
    },
}

const MOUNT_ENUMERATION_MOUNT: &str = "";

/// The renderer-neutral, async-first projection core.
pub struct Tree {
    mounts: Mounts,
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
    /// Production constructor: the registry both renderers already hold.
    pub fn new(registry: Arc<ProviderRegistry>) -> Self {
        Self {
            mounts: Mounts::Registry(registry),
        }
    }

    /// Test/shim constructor for the kernel-free itest and any single-mount
    /// embedding. Wraps a bare `Arc<Runtime>` under a single mount name so a
    /// `Tree` is drivable without building a full `ProviderRegistry`.
    pub fn for_runtime(runtime: Arc<Runtime>, mount: impl Into<String>) -> Self {
        Self {
            mounts: Mounts::Single {
                mount: mount.into(),
                runtime,
            },
        }
    }

    /// The runtime serving `mount`, or an error if no such mount exists.
    pub(crate) fn runtime_for(&self, mount: &str) -> Result<Arc<Runtime>> {
        match &self.mounts {
            Mounts::Single { mount: m, runtime } if m == mount => Ok(Arc::clone(runtime)),
            Mounts::Single { mount: m, .. } => Err(TreeError::not_found(format!(
                "no such mount: {mount} (single-mount tree serves {m})"
            ))),
            Mounts::Registry(registry) => registry
                .get(mount)
                .ok_or_else(|| TreeError::not_found(format!("no such mount: {mount}"))),
        }
    }

    /// The runtime serving `mount` if present, without erroring. Used by the
    /// sync invalidation drain, which must no-op on an unknown mount.
    pub(crate) fn registry_runtime(&self, mount: &str) -> Option<Arc<Runtime>> {
        match &self.mounts {
            Mounts::Single { mount: m, runtime } if m == mount => Some(Arc::clone(runtime)),
            Mounts::Single { .. } => None,
            Mounts::Registry(registry) => registry.get(mount),
        }
    }

    /// Split a full protocol path into (mount_name, mount-relative path).
    ///
    /// For a single-mount tree the mount is fixed and the whole input path is
    /// mount-relative (the itest drives mount-relative paths like "/" and
    /// "/hello"). For a registry-backed tree the mount is the first path
    /// segment; the remainder (with a leading slash) is mount-relative. The
    /// synthetic mount-enumeration root (a bare "/" against a registry) is
    /// designed here but only the single-mount arm is exercised in slice 1.
    pub(crate) fn split_mount_path(&self, path: &Path) -> Result<(String, Path)> {
        match &self.mounts {
            Mounts::Single { mount, .. } => Ok((mount.clone(), path.clone())),
            Mounts::Registry(registry) => {
                // A root-mounted provider claims the whole namespace.
                if let Some(root) = registry.root_mount_name() {
                    return Ok((root, path.clone()));
                }
                if path.is_root() {
                    return Ok((MOUNT_ENUMERATION_MOUNT.to_string(), Path::root()));
                }
                let mut segments = path.segments();
                let Some(mount) = segments.next() else {
                    return Err(TreeError::invalid_input(format!(
                        "split_mount_path: empty path: {}",
                        path.as_str()
                    )));
                };
                let mount = mount.to_string();
                if !registry.mounts().iter().any(|m| m == &mount) {
                    return Err(TreeError::not_found(format!("no such mount: {mount}")));
                }
                let rest = path
                    .as_str()
                    .strip_prefix(&format!("/{mount}"))
                    .filter(|s| !s.is_empty())
                    .unwrap_or("/");
                let rel = Path::parse(rest).map_err(|e| {
                    TreeError::invalid_input(format!("invalid mount-relative path: {e}"))
                })?;
                Ok((mount, rel))
            },
        }
    }

    pub(crate) fn is_mount_enumeration_root(&self, mount: &str, path: &Path) -> bool {
        matches!(&self.mounts, Mounts::Registry(registry) if registry.root_mount_name().is_none())
            && mount == MOUNT_ENUMERATION_MOUNT
            && path.is_root()
    }

    pub(crate) fn mount_names(&self) -> Option<Vec<String>> {
        match &self.mounts {
            Mounts::Registry(registry) if registry.root_mount_name().is_none() => {
                let mut mounts = registry.mounts();
                mounts.sort();
                Some(mounts)
            },
            Mounts::Registry(_) | Mounts::Single { .. } => None,
        }
    }
}
