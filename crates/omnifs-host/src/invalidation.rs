use super::Runtime;
use omnifs_core::path::Path;
use parking_lot::Mutex;

#[derive(Default)]
pub(super) struct InvalidationState {
    invalidated_prefixes: Mutex<Vec<String>>,
    invalidated_paths: Mutex<Vec<String>>,
    changed_dirs: Mutex<Vec<String>>,
}

impl InvalidationState {
    pub(super) fn record_prefix(&self, prefix: String) {
        self.invalidated_prefixes.lock().push(prefix);
    }

    pub(super) fn record_path(&self, path: String) {
        self.invalidated_paths.lock().push(path);
    }

    pub(super) fn record_changed_dir(&self, path: String) {
        self.changed_dirs.lock().push(path);
    }

    fn drain_prefixes(&self) -> Vec<String> {
        let mut prefixes = self.invalidated_prefixes.lock();
        std::mem::take(&mut *prefixes)
    }

    fn drain_paths(&self) -> Vec<String> {
        let mut paths = self.invalidated_paths.lock();
        std::mem::take(&mut *paths)
    }

    fn drain_changed_dirs(&self) -> Vec<String> {
        let mut paths = self.changed_dirs.lock();
        std::mem::take(&mut *paths)
    }
}

impl Runtime {
    pub fn cache_delete_prefix(&self, prefix: &str) {
        let prefix = protocol_path(prefix);
        self.cache.delete_listing_prefix(&prefix);
        self.invalidation.record_prefix(prefix.to_string());
    }

    pub fn cache_delete_path(&self, path: &str) {
        let path = protocol_path(path);
        self.cache.delete_listing_path(&path);
        self.invalidation.record_path(path.to_string());
    }

    /// Record that directory `path`'s contents changed without touching the
    /// cached view record. Used by pagination's `@next`/`@all`; the host/FUSE
    /// frontend drains this event and decides how to notify the kernel.
    pub fn record_dir_changed(&self, path: &str) {
        self.invalidation.record_changed_dir(path.to_string());
    }

    pub fn drain_invalidated_prefixes(&self) -> Vec<String> {
        self.invalidation.drain_prefixes()
    }

    pub fn drain_invalidated_paths(&self) -> Vec<String> {
        self.invalidation.drain_paths()
    }

    pub fn drain_changed_dirs(&self) -> Vec<String> {
        self.invalidation.drain_changed_dirs()
    }

    pub(super) fn record_view_invalidations(&self, prefixes: Vec<String>, paths: Vec<String>) {
        for prefix in prefixes {
            self.invalidation.record_prefix(prefix);
        }
        for path in paths {
            self.invalidation.record_path(path);
        }
    }
}

fn protocol_path(path: &str) -> Path {
    Path::parse(path).expect("invalidation path must be a protocol path")
}
