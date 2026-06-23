//! Invalidation report types and the sync `Tree::drain_invalidations` body.

use omnifs_core::path::Path;

use crate::error::Result;
use crate::node::Node;
use crate::{RequestCtx, Tree};

/// Neutral half of the invalidation fan-out. `Tree` has already done its own mem
/// eviction; the renderer consumes this to drive its kernel notifier (FUSE
/// inval_entry/inval_inode) and prune its inode/filehandle/stateid/negative
/// tables. WatchStream precursor: in the liveness phase, `watch()` pushes this
/// instead of the renderer pulling it via `drain_invalidations`.
#[derive(Debug, Clone, Default)]
pub struct InvalidationReport {
    pub paths: Vec<Path>,
    pub prefixes: Vec<Path>,
    pub changed_dirs: Vec<Path>,
}

impl InvalidationReport {
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty() && self.prefixes.is_empty() && self.changed_dirs.is_empty()
    }
}

/// LIVENESS PHASE, NOT BUILT IN SLICE 1. Opaque so the `watch()` method
/// compiles. Becomes a `futures::Stream<Item = InvalidationReport>` when
/// liveness lands.
pub struct WatchStream(pub(crate) ());

impl Tree {
    /// LIVENESS PHASE, NOT NOW. SLICE 1: `todo!()`. Its signal already exists as
    /// `drain_invalidations` (pull); `watch` pushes the same
    /// `InvalidationReport` instead of polling.
    // Async surface is intentional (liveness body awaits the watch source).
    #[allow(clippy::unused_async)]
    pub async fn watch(&self, _node: &Node, _ctx: &RequestCtx) -> Result<WatchStream> {
        todo!("liveness phase: push InvalidationReport as a WatchStream")
    }

    /// SYNC drain of pending runtime invalidations for a mount. Does the
    /// Tree-owned mem eviction (`Runtime::mem_invalidate_entries_if`) and
    /// returns the neutral `InvalidationReport` so the renderer drives its own
    /// kernel notifier + prunes its inode/filehandle/stateid tables. NOT async:
    /// the underlying `Runtime::drain_invalidated_{prefixes,paths}` /
    /// `drain_changed_dirs` are sync queue drains touching no provider. Both
    /// renderers call this at the top of each op. This is the `watch()`-less
    /// first-phase invalidation API.
    pub fn drain_invalidations(&self, mount: &str) -> InvalidationReport {
        let Some(runtime) = self.registry_runtime(mount) else {
            return InvalidationReport::default();
        };

        let prefixes = runtime.drain_invalidated_prefixes();
        let paths = runtime.drain_invalidated_paths();
        let changed_dirs = runtime.drain_changed_dirs();

        if !(prefixes.is_empty() && paths.is_empty() && changed_dirs.is_empty()) {
            // Tree-owned mem eviction; the kernel-notify half stays
            // renderer-side and consumes the returned report.
            runtime.cache().mem_invalidate_entries_if({
                let paths = paths.clone();
                let prefixes = prefixes.clone();
                move |k, _| {
                    paths.contains(&k.path)
                        || prefixes.iter().any(|prefix| k.path.has_prefix(prefix))
                }
            });
        }

        InvalidationReport {
            paths,
            prefixes,
            changed_dirs,
        }
    }
}
