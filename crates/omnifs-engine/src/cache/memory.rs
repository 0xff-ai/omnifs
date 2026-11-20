//! Derived in-memory projection records.

use super::mount::{Key, Record, RecordKind};
use moka::ops::compute::Op;
use moka::sync::Cache as MokaCache;
use omnifs_core::path::Path;
use std::collections::HashMap;
use std::sync::Arc;

/// Maximum total byte weight of the `mem` tier per provider instance (32 MiB).
pub const VIEW_MEM_MAX_WEIGHT: u64 = 32 * 1024 * 1024;

/// Records larger than this threshold are not inserted into `mem` (256 KiB).
pub const VIEW_MEM_SKIP_THRESHOLD: usize = 256 * 1024;

/// The records of one path, by their kind and auxiliary key.
type PathRecords = HashMap<(RecordKind, Option<String>), Arc<Record>>;

/// Derived Moka records, by path, so that the removal of the records of a
/// path is one removal by its key. Durable facts are owned by
/// `ProjectionStore`.
pub struct MemoryTier {
    mem: MokaCache<Path, Arc<PathRecords>>,
}

impl MemoryTier {
    pub fn new() -> Self {
        Self {
            mem: Self::build_mem(),
        }
    }

    fn build_mem() -> MokaCache<Path, Arc<PathRecords>> {
        MokaCache::builder()
            .max_capacity(VIEW_MEM_MAX_WEIGHT)
            .support_invalidation_closures()
            .weigher(|path: &Path, records: &Arc<PathRecords>| -> u32 {
                let size: usize = records
                    .iter()
                    .map(|((_, aux), record)| {
                        3 + path.len() + aux.as_ref().map_or(0, String::len) + record.payload.len()
                    })
                    .sum();
                size.try_into().unwrap_or(u32::MAX)
            })
            .build()
    }

    // --- Derived memory operations ---------------------------------------

    pub fn mem_get(&self, key: &Key) -> Option<Arc<Record>> {
        self.mem
            .get(&key.path)?
            .get(&(key.kind, key.aux.clone()))
            .cloned()
    }

    pub fn mem_put(&self, key: &Key, record: &Record) {
        if record.payload.len() <= VIEW_MEM_SKIP_THRESHOLD {
            self.mem.entry_by_ref(&key.path).and_upsert_with(|current| {
                let mut records =
                    current.map_or_else(PathRecords::new, |entry| (**entry.value()).clone());
                records.insert((key.kind, key.aux.clone()), Arc::new(record.clone()));
                Arc::new(records)
            });
        }
    }

    /// Remove the exact key from the mem only. Does not touch the database.
    pub fn mem_invalidate(&self, key: &Key) {
        self.mem
            .entry_by_ref(&key.path)
            .and_compute_with(|current| {
                let Some(entry) = current else {
                    return Op::Nop;
                };
                let mut records = (**entry.value()).clone();
                records.remove(&(key.kind, key.aux.clone()));
                if records.is_empty() {
                    Op::Remove
                } else {
                    Op::Put(Arc::new(records))
                }
            });
    }

    /// Remove the mem records of all paths matching `predicate`. Does not
    /// touch the database.
    pub fn mem_invalidate_paths_if<P>(&self, predicate: P)
    where
        P: Fn(&Path) -> bool + Send + Sync + 'static,
    {
        self.mem
            .invalidate_entries_if(move |path, _| predicate(path))
            .expect("invalidation closures enabled at cache construction");
    }

    /// Remove all mem records at `prefix` or beneath it on a segment
    /// boundary.
    ///
    /// `prefix` may be a mount-scoped or unscoped omnifs path. Matching uses
    /// typed path segment boundaries.
    pub fn invalidate_prefix(&self, prefix: &Path) {
        let prefix_owned = prefix.clone();
        self.mem
            .invalidate_entries_if(move |path, _| path.has_prefix(&prefix_owned))
            .expect("invalidation closures enabled at cache construction");
        // Flush pending maintenance so the predicate is applied immediately
        // (moka applies invalidate_entries_if lazily otherwise).
        self.mem.run_pending_tasks();
    }

    /// Remove all mem records whose logical path equals `path`.
    pub fn delete_exact(&self, path: &Path) {
        self.mem.invalidate(path);
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
        Key::with_aux(
            &Path::parse(path).unwrap(),
            RecordKind::Attr,
            None::<String>,
        )
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

        let file = Key::with_aux(
            &Path::parse("/owner/repobaz").unwrap(),
            RecordKind::File,
            Some("aux"),
        );
        memory.mem_put(&file, &small);
        memory.mem_invalidate(&key("/owner/repobaz"));
        assert!(memory.mem_get(&key("/owner/repobaz")).is_none());
        assert!(memory.mem_get(&file).is_some());
        memory.mem_put(&key("/owner/repobaz"), &small);
        memory.delete_exact(&Path::parse("/owner/repobaz").unwrap());
        assert!(memory.mem_get(&key("/owner/repobaz")).is_none());
        assert!(memory.mem_get(&file).is_none());
    }
}
