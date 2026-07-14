//! Invalidation report types and the sync `Tree::drain_invalidations` body.

use omnifs_api::events::CacheKind;
use omnifs_core::path::Path;

use crate::Tree;

/// Neutral half of the invalidation fan-out. `Tree` has already done its own mem
/// eviction; the renderer consumes this to drive its kernel notifier (FUSE
/// inval_entry/inval_inode) and prune its inode/filehandle/stateid/negative
/// tables.
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

impl Tree {
    /// SYNC drain of pending runtime invalidations for a mount. Does the
    /// Tree-owned mem eviction (`Runtime::mem_invalidate_entries_if`) and
    /// returns the neutral `InvalidationReport` so the renderer drives its own
    /// kernel notifier + prunes its inode/filehandle/stateid tables. NOT async:
    /// the underlying `Runtime::drain_invalidated_{prefixes,paths}` /
    /// `drain_changed_dirs` are sync queue drains touching no provider. Both
    /// renderers call this at the top of each op. This is the shared pull-based
    /// invalidation API.
    pub fn drain_invalidations(&self, mount: &str) -> InvalidationReport {
        let Some(runtime) = self.ctx.registry_runtime(mount) else {
            return InvalidationReport::default();
        };

        let prefixes = runtime.drain_invalidated_prefixes();
        let paths = runtime.drain_invalidated_paths();
        let changed_dirs = runtime.drain_changed_dirs();

        if !(prefixes.is_empty() && paths.is_empty() && changed_dirs.is_empty()) {
            crate::inspector::cache_event(CacheKind::Invalidated);
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
