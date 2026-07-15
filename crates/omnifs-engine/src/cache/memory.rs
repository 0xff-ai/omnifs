//! Derived in-memory projection records.

use super::mount::{Key, Record};
use moka::sync::Cache as MokaCache;
use omnifs_core::path::Path;
use std::sync::Arc;

/// Maximum total byte weight of the `mem` tier per provider instance (32 MiB).
pub const VIEW_MEM_MAX_WEIGHT: u64 = 32 * 1024 * 1024;

/// Records larger than this threshold are not inserted into `mem` (256 KiB).
pub const VIEW_MEM_SKIP_THRESHOLD: usize = 256 * 1024;

/// Derived Moka records. Durable facts are owned by `ProjectionStore`.
pub struct MemoryTier {
    mem: MokaCache<Key, Arc<Record>>,
}

impl MemoryTier {
    pub fn new() -> Self {
        Self {
            mem: Self::build_mem(),
        }
    }

    fn build_mem() -> MokaCache<Key, Arc<Record>> {
        MokaCache::builder()
            .max_capacity(VIEW_MEM_MAX_WEIGHT)
            .support_invalidation_closures()
            .weigher(|key: &Key, value: &Arc<Record>| -> u32 {
                let key_size = 1 + key.path.len() + key.aux.as_ref().map_or(0, String::len);
                let val_size = 2 + value.payload.len();
                (key_size + val_size).try_into().unwrap_or(u32::MAX)
            })
            .build()
    }

    // --- Derived memory operations ---------------------------------------

    pub fn mem_get(&self, key: &Key) -> Option<Arc<Record>> {
        self.mem.get(key)
    }

    pub fn mem_put(&self, key: &Key, record: &Record) {
        if record.payload.len() <= VIEW_MEM_SKIP_THRESHOLD {
            self.mem.insert(key.clone(), Arc::new(record.clone()));
        }
    }

    /// Remove the exact key from the mem only. Does not touch the database.
    pub fn mem_invalidate(&self, key: &Key) {
        self.mem.invalidate(key);
    }

    /// Remove all mem entries matching `predicate`. Does not touch the
    /// database.
    pub fn mem_invalidate_entries_if<P>(&self, predicate: P)
    where
        P: Fn(&Key, &Arc<Record>) -> bool + Send + Sync + 'static,
    {
        self.mem
            .invalidate_entries_if(predicate)
            .expect("invalidation closures enabled at cache construction");
    }

    /// Remove all records at `prefix` or beneath it on a segment boundary from
    /// both the mem and the database.
    ///
    /// `prefix` may be a mount-scoped or unscoped omnifs path. Matching uses
    /// typed path segment boundaries.
    pub fn invalidate_prefix(&self, prefix: &Path) {
        // Mem: use predicate-based eviction on path prefix.
        let prefix_owned = prefix.clone();
        self.mem
            .invalidate_entries_if(move |k, _| k.path.has_prefix(&prefix_owned))
            .expect("invalidation closures enabled at cache construction");
        // Flush pending maintenance so the predicate is applied immediately
        // (moka applies invalidate_entries_if lazily otherwise).
        self.mem.run_pending_tasks();
    }

    /// Remove all records whose logical path equals `path` from both tiers.
    pub fn delete_exact(&self, path: &Path) {
        // Mem: exact-path eviction across all record kinds.
        let path_owned = path.clone();
        self.mem
            .invalidate_entries_if(move |k, _| k.path == path_owned)
            .expect("invalidation closures enabled at cache construction");
        // Flush pending maintenance so the predicate is applied immediately.
        self.mem.run_pending_tasks();
    }
}

impl Default for MemoryTier {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{MemoryTier, VIEW_MEM_SKIP_THRESHOLD};
    use crate::cache::mount::{Key, Record, RecordKind};
    use omnifs_core::path::Path;

    fn key(path: &str) -> Key {
        Key::new(&Path::parse(path).unwrap(), RecordKind::Attr)
    }

    #[test]
    fn skips_large_records_and_respects_segment_boundaries() {
        let memory = MemoryTier::new();
        let small = Record::new(RecordKind::Attr, vec![1]);
        memory.mem_put(&key("/small"), &small);
        assert!(memory.mem_get(&key("/small")).is_some());

        let large = Record::new(RecordKind::Attr, vec![0; VIEW_MEM_SKIP_THRESHOLD + 1]);
        memory.mem_put(&key("/large"), &large);
        assert!(memory.mem_get(&key("/large")).is_none());

        memory.mem_put(&key("/owner/repo"), &small);
        memory.mem_put(&key("/owner/repo/issues"), &small);
        memory.mem_put(&key("/owner/repobaz"), &small);
        memory.invalidate_prefix(&Path::parse("/owner/repo").unwrap());
        assert!(memory.mem_get(&key("/owner/repo")).is_none());
        assert!(memory.mem_get(&key("/owner/repo/issues")).is_none());
        assert!(memory.mem_get(&key("/owner/repobaz")).is_some());
    }
}
