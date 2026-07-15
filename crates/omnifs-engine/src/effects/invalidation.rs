use crate::Runtime;
use omnifs_core::path::Path;
use parking_lot::Mutex;

#[derive(Default)]
pub(crate) struct InvalidationState {
    invalidated_prefixes: Mutex<Vec<Path>>,
    invalidated_paths: Mutex<Vec<Path>>,
    changed_dirs: Mutex<Vec<Path>>,
}

impl InvalidationState {
    pub(crate) fn record_prefix(&self, prefix: Path) {
        self.invalidated_prefixes.lock().push(prefix);
    }

    pub(crate) fn record_path(&self, path: Path) {
        self.invalidated_paths.lock().push(path);
    }

    pub(crate) fn record_changed_dir(&self, path: Path) {
        self.changed_dirs.lock().push(path);
    }

    fn drain_prefixes(&self) -> Vec<Path> {
        let mut prefixes = self.invalidated_prefixes.lock();
        std::mem::take(&mut *prefixes)
    }

    fn drain_paths(&self) -> Vec<Path> {
        let mut paths = self.invalidated_paths.lock();
        std::mem::take(&mut *paths)
    }

    fn drain_changed_dirs(&self) -> Vec<Path> {
        let mut paths = self.changed_dirs.lock();
        std::mem::take(&mut *paths)
    }
}

impl Runtime {
    pub fn cache_delete_prefix(&self, prefix: &Path) {
        self.resources.memory.invalidate_prefix(prefix);
        self.invalidation.record_prefix(prefix.clone());
    }

    pub fn cache_delete_path(&self, path: &Path) {
        self.resources.memory.delete_exact(path);
        self.invalidation.record_path(path.clone());
    }

    /// Record that directory `path`'s contents changed without touching the
    /// cached view record. Used by pagination's `@next`/`@all`; the host/FUSE
    /// frontend drains this event and decides how to notify the kernel.
    pub fn record_dir_changed(&self, path: &Path) {
        self.invalidation.record_changed_dir(path.clone());
    }

    pub fn drain_invalidated_prefixes(&self) -> Vec<Path> {
        self.invalidation.drain_prefixes()
    }

    pub fn drain_invalidated_paths(&self) -> Vec<Path> {
        self.invalidation.drain_paths()
    }

    pub fn drain_changed_dirs(&self) -> Vec<Path> {
        self.invalidation.drain_changed_dirs()
    }

    pub(crate) fn record_view_invalidations(&self, prefixes: Vec<Path>, paths: Vec<Path>) {
        for prefix in prefixes {
            self.invalidation.record_prefix(prefix);
        }
        for path in paths {
            self.invalidation.record_path(path);
        }
    }
}
