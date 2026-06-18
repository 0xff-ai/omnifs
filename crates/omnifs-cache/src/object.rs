//! Object cache : durable, global, ObjectId-keyed canonical bytes.
//!
//! Backed by a redb database with two tables (byte keys throughout):
//! - `objects`: `mount\x1f{id}` → postcard of `StoredObject`
//! - `paths`:   `mount\x1f{full-path}` → scoped `ObjectId` bytes
//!
//! The cache is mount-agnostic; all keys are pre-scoped by the caller
//! (`Store::scoped_id` / `Store::scoped_path_bytes`). The per-mount generation
//! fence lives in `Store`.

use crate::write_txn;
use anyhow::Result;
#[allow(unused_imports)]
use redb::ReadableTable as _;
use redb::{Database, TableDefinition};
use std::path::Path;
use std::sync::Arc;

/// On-disk schema version for `StoredObject`. Bump on layout change.
pub const SCHEMA: u8 = 1;

const OBJECTS_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("objects");
const PATHS_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("paths");

/// Canonical bytes for one object.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Canonical {
    pub bytes: Vec<u8>,
    pub validator: Option<String>,
}

/// One object row stored in the database.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StoredObject {
    pub schema: u8,
    pub id: Vec<u8>,
    pub canonical: Option<Canonical>,
    pub leaves: Vec<String>,
}

impl StoredObject {
    fn new(scoped_id: &[u8], canonical: Option<Canonical>, leaves: Vec<String>) -> Self {
        Self {
            schema: SCHEMA,
            id: scoped_id.to_vec(),
            canonical,
            leaves,
        }
    }
}

/// One entry in a `Cache::store_batch` call. Fence checks and view evictions
/// are the caller's responsibility; this type carries pre-validated data.
pub struct StoreBatchEntry {
    pub scoped_id: Vec<u8>,
    pub canonical: Canonical,
    pub new_leaves: Vec<String>,
}

/// Global, durable object-id cache. One instance per process; mount isolation
/// is enforced by the `mount\x1f` key prefix injected by `Store`.
pub struct Cache {
    disk: Arc<Database>,
}

impl Cache {
    /// Open the durable object database at `path`, creating it if absent.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let disk = Database::create(path)?;
        let txn = disk.begin_write()?;
        {
            let _ = txn.open_table(OBJECTS_TABLE)?;
            let _ = txn.open_table(PATHS_TABLE)?;
        }
        txn.commit()?;
        Ok(Self {
            disk: Arc::new(disk),
        })
    }

    /// UPSERT: union `new_leaves`, set canonical, keep existing PATHS rows, add
    /// rows for new leaves, evict rendered view bytes for current leaves first.
    pub fn store(
        &self,
        scoped_id: &[u8],
        canonical: Canonical,
        new_leaves: &[String],
        mut view_evict: impl FnMut(&str),
    ) -> bool {
        let prior_leaves = self.leaves_of(scoped_id);
        for leaf in &prior_leaves {
            view_evict(leaf);
        }

        let merged_leaves = merge_leaves(&prior_leaves, new_leaves);
        let stored = StoredObject::new(scoped_id, Some(canonical), merged_leaves);

        self.commit_object(scoped_id, &stored, new_leaves)
    }

    /// Index-only upsert for preload fs-writes. Canonical-beats-preload: an
    /// existing `Some` canonical is never clobbered to `None`.
    pub fn store_index_only(&self, scoped_id: &[u8], new_leaves: &[String]) -> bool {
        let existing = self.get(scoped_id);
        let (canonical, base_leaves) = match existing {
            Some(obj) => (obj.canonical, obj.leaves),
            None => (None, Vec::new()),
        };
        let merged_leaves = merge_leaves(&base_leaves, new_leaves);
        let stored = StoredObject::new(scoped_id, canonical, merged_leaves);
        self.commit_object(scoped_id, &stored, new_leaves)
    }

    /// Batch canonical store: commits all entries in ONE redb write transaction.
    ///
    /// Fence checks and prior-leaf view evictions are the caller's responsibility
    /// (done before this call so they can be rejected individually without
    /// aborting the batch). Each entry reads existing leaves, merges, serializes,
    /// then all writes land in one commit.
    pub fn store_batch(&self, entries: &[StoreBatchEntry]) {
        if entries.is_empty() {
            return;
        }

        // Read phase: collect prior leaves + build StoredObject for each entry.
        let prepared: Vec<(Vec<u8>, Vec<u8>)> = entries
            .iter()
            .filter_map(|e| {
                let prior_leaves = self.leaves_of(&e.scoped_id);
                let merged_leaves = merge_leaves(&prior_leaves, &e.new_leaves);
                let stored =
                    StoredObject::new(&e.scoped_id, Some(e.canonical.clone()), merged_leaves);
                let payload = match postcard::to_allocvec(&stored) {
                    Ok(p) => p,
                    Err(err) => {
                        tracing::warn!(
                            error = %err,
                            "object cache: batch serialize failed; skipping entry"
                        );
                        return None;
                    },
                };
                Some((e.scoped_id.clone(), payload))
            })
            .collect();

        if prepared.is_empty() {
            return;
        }

        if let Err(e) = write_txn(&self.disk, |txn| {
            let mut objects = txn.open_table(OBJECTS_TABLE)?;
            let mut paths = txn.open_table(PATHS_TABLE)?;
            for ((scoped_id, payload), entry) in prepared.iter().zip(entries.iter()) {
                objects.insert(scoped_id.as_slice(), payload.as_slice())?;
                for leaf in &entry.new_leaves {
                    paths.insert(leaf.as_bytes(), scoped_id.as_slice())?;
                }
            }
            Ok(())
        }) {
            tracing::warn!(error = %e, "object cache: batch write failed");
        }
    }

    pub fn get(&self, scoped_id: &[u8]) -> Option<StoredObject> {
        let txn = self.disk.begin_read().ok()?;
        let table = txn.open_table(OBJECTS_TABLE).ok()?;
        let guard = table.get(scoped_id).ok()??;
        decode_object(guard.value())
    }

    /// Forward index: scoped full path → scoped `ObjectId` bytes.
    pub fn id_of(&self, scoped_path: &[u8]) -> Option<Vec<u8>> {
        let txn = self.disk.begin_read().ok()?;
        let table = txn.open_table(PATHS_TABLE).ok()?;
        let guard = table.get(scoped_path).ok()??;
        Some(guard.value().to_vec())
    }

    /// Alias set for `scoped_id` (full scoped view-leaf paths). Empty when absent.
    pub fn leaves_of(&self, scoped_id: &[u8]) -> Vec<String> {
        self.get(scoped_id)
            .map(|obj| obj.leaves)
            .unwrap_or_default()
    }

    /// Full eviction: OBJECTS row, every PATHS row in the alias set, and view leaves.
    pub fn evict_object(&self, scoped_id: &[u8], mut view_evict: impl FnMut(&str)) {
        let leaves = self.leaves_of(scoped_id);
        for leaf in &leaves {
            view_evict(leaf);
        }

        if let Err(e) = write_txn(&self.disk, |txn| {
            let mut objects = txn.open_table(OBJECTS_TABLE)?;
            let mut paths = txn.open_table(PATHS_TABLE)?;
            objects.remove(scoped_id)?;
            for leaf in &leaves {
                paths.remove(leaf.as_bytes())?;
            }
            Ok(())
        }) {
            tracing::warn!(error = %e, "object cache: evict_object failed");
        }
    }

    /// Capacity eviction: drop canonical bytes + validator and evict rendered view
    /// leaves, but keep the OBJECTS row (canonical=None) and all PATHS rows.
    pub fn capacity_evict(&self, scoped_id: &[u8], mut view_evict: impl FnMut(&str)) {
        let Some(mut obj) = self.get(scoped_id) else {
            return;
        };
        for leaf in &obj.leaves {
            view_evict(leaf);
        }
        obj.canonical = None;

        if let Err(e) = write_txn(&self.disk, |txn| {
            let payload = postcard::to_allocvec(&obj)?;
            let mut objects = txn.open_table(OBJECTS_TABLE)?;
            objects.insert(scoped_id, payload.as_slice())?;
            Ok(())
        }) {
            tracing::warn!(error = %e, "object cache: capacity_evict failed");
        }
    }

    fn commit_object(
        &self,
        scoped_id: &[u8],
        stored: &StoredObject,
        new_leaves: &[String],
    ) -> bool {
        let payload = match postcard::to_allocvec(stored) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "object cache: serialize failed");
                return false;
            },
        };

        match write_txn(&self.disk, |txn| {
            let mut objects = txn.open_table(OBJECTS_TABLE)?;
            let mut paths = txn.open_table(PATHS_TABLE)?;
            objects.insert(scoped_id, payload.as_slice())?;
            for leaf in new_leaves {
                paths.insert(leaf.as_bytes(), scoped_id)?;
            }
            Ok(())
        }) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(error = %e, "object cache: write failed");
                false
            },
        }
    }
}

fn decode_object(bytes: &[u8]) -> Option<StoredObject> {
    let obj: StoredObject = postcard::from_bytes(bytes).ok()?;
    if obj.schema != SCHEMA {
        return None;
    }
    Some(obj)
}

fn merge_leaves(existing: &[String], new_leaves: &[String]) -> Vec<String> {
    // Build a set over existing entries to avoid O(n²) scanning; new leaves
    // are deduplicated in arrival order (first occurrence kept).
    let mut seen: std::collections::HashSet<&str> = existing.iter().map(String::as_str).collect();
    let mut merged = existing.to_vec();
    for leaf in new_leaves {
        if seen.insert(leaf.as_str()) {
            merged.push(leaf.clone());
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scoped_id(mount: &str, id: &[u8]) -> Vec<u8> {
        let mut key = mount.as_bytes().to_vec();
        key.push(0x1f);
        key.extend_from_slice(id);
        key
    }

    fn scoped_path(mount: &str, path: &str) -> Vec<u8> {
        let mut key = mount.as_bytes().to_vec();
        key.push(0x1f);
        key.extend_from_slice(path.as_bytes());
        key
    }

    fn open_cache() -> (tempfile::TempDir, Cache) {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(&dir.path().join("object.redb")).unwrap();
        (dir, cache)
    }

    #[test]
    fn canonical_beats_preload() {
        let (_dir, cache) = open_cache();
        let id = scoped_id("m", b"issue:42");
        let l1 = "m\x1f/issues/open/42/item.json".to_string();
        let c = Canonical {
            bytes: b"data".to_vec(),
            validator: Some("v1".to_string()),
        };
        assert!(cache.store(&id, c, std::slice::from_ref(&l1), |_| {}));

        let l3 = "m\x1f/issues/open/42/title".to_string();
        assert!(cache.store_index_only(&id, &[l3]));

        let got = cache.get(&id).unwrap();
        assert!(got.canonical.is_some());
        assert_eq!(got.canonical.as_ref().unwrap().bytes, b"data");
        assert!(got.leaves.contains(&l1));
        assert!(
            got.leaves
                .contains(&"m\x1f/issues/open/42/title".to_string())
        );
    }

    #[test]
    fn overwrite_unions_aliases_and_evicts_prior_views() {
        let (_dir, cache) = open_cache();
        let id = scoped_id("m", b"x");
        let l1 = "m\x1f/p/L1".to_string();
        let l2 = "m\x1f/p/L2".to_string();

        cache.store(
            &id,
            Canonical {
                bytes: b"v1".to_vec(),
                validator: None,
            },
            std::slice::from_ref(&l1),
            |_| {},
        );

        let mut evicted = Vec::new();
        cache.store(
            &id,
            Canonical {
                bytes: b"v2".to_vec(),
                validator: None,
            },
            std::slice::from_ref(&l2),
            |leaf| evicted.push(leaf.to_string()),
        );

        let got = cache.get(&id).unwrap();
        assert!(got.leaves.contains(&l1));
        assert!(got.leaves.contains(&l2));
        assert_eq!(cache.id_of(l1.as_bytes()).as_deref(), Some(id.as_slice()));
        assert_eq!(got.canonical.unwrap().bytes, b"v2");

        let mut evicted_sorted = evicted;
        evicted_sorted.sort();
        assert_eq!(evicted_sorted, vec![l1]);
    }

    #[test]
    fn capacity_evict_keeps_index_drops_validator() {
        let (_dir, cache) = open_cache();
        let id = scoped_id("m", b"x");
        let leaf = "m\x1f/a/leaf".to_string();
        cache.store(
            &id,
            Canonical {
                bytes: b"data".to_vec(),
                validator: Some("etag".to_string()),
            },
            std::slice::from_ref(&leaf),
            |_| {},
        );

        cache.capacity_evict(&id, |_| {});

        let got = cache.get(&id).unwrap();
        assert!(got.canonical.is_none());
        assert_eq!(cache.id_of(leaf.as_bytes()).as_deref(), Some(id.as_slice()));
    }

    #[test]
    fn evict_object_removes_object_and_paths() {
        let (_dir, cache) = open_cache();
        let id = scoped_id("m", b"x");
        let leaf = "m\x1f/a/leaf".to_string();
        cache.store(
            &id,
            Canonical {
                bytes: b"data".to_vec(),
                validator: None,
            },
            std::slice::from_ref(&leaf),
            |_| {},
        );

        cache.evict_object(&id, |_| {});

        assert!(cache.get(&id).is_none());
        assert!(cache.id_of(leaf.as_bytes()).is_none());
    }

    #[test]
    fn id_of_exact_lookup() {
        let (_dir, cache) = open_cache();
        let id = scoped_id("m", b"obj");
        let p1 = scoped_path("m", "/issues/42/item.md");
        let p2 = scoped_path("m", "/issues/42/title");
        cache.store(
            &id,
            Canonical {
                bytes: b"data".to_vec(),
                validator: None,
            },
            &[
                String::from_utf8(p1.clone()).unwrap(),
                String::from_utf8(p2.clone()).unwrap(),
            ],
            |_| {},
        );

        assert_eq!(cache.id_of(&p1).as_deref(), Some(id.as_slice()));
        assert_eq!(cache.id_of(&p2).as_deref(), Some(id.as_slice()));
        assert!(
            cache
                .id_of(scoped_path("m", "/issues/42/other").as_slice())
                .is_none()
        );
    }

    /// Batch-put of N objects yields identical observable state (`get`/`id_of`/`leaves_of`)
    /// to N individual single puts, including a mixed case with one fence-rejected entry.
    #[test]
    fn store_batch_equivalent_to_single_puts() {
        let (_dir_a, cache_a) = open_cache();
        let (_dir_b, cache_b) = open_cache();

        let id1 = scoped_id("m", b"obj:1");
        let id2 = scoped_id("m", b"obj:2");
        let id3 = scoped_id("m", b"obj:3"); // will be "fence-rejected" by the caller

        let l1a = "m\x1f/issues/1/item.json".to_string();
        let l1b = "m\x1f/issues/all/1/item.json".to_string();
        let l2 = "m\x1f/issues/2/item.json".to_string();
        let l3 = "m\x1f/issues/3/item.json".to_string();

        // Single-put baseline: obj:3 is intentionally omitted (simulates rejection).
        cache_a.store(
            &id1,
            Canonical {
                bytes: b"payload1".to_vec(),
                validator: Some("v1".to_string()),
            },
            &[l1a.clone(), l1b.clone()],
            |_| {},
        );
        cache_a.store(
            &id2,
            Canonical {
                bytes: b"payload2".to_vec(),
                validator: None,
            },
            std::slice::from_ref(&l2),
            |_| {},
        );

        // Batch-put equivalent (obj:3 excluded, same as single-put baseline).
        cache_b.store_batch(&[
            StoreBatchEntry {
                scoped_id: id1.clone(),
                canonical: Canonical {
                    bytes: b"payload1".to_vec(),
                    validator: Some("v1".to_string()),
                },
                new_leaves: vec![l1a.clone(), l1b.clone()],
            },
            StoreBatchEntry {
                scoped_id: id2.clone(),
                canonical: Canonical {
                    bytes: b"payload2".to_vec(),
                    validator: None,
                },
                new_leaves: vec![l2.clone()],
            },
        ]);
        // obj:3 not included — caller (Store fence) would have dropped it.
        let _ = (&id3, &l3); // suppress unused warnings

        // Verify identical observable state.
        for (desc, ca, cb, id, leaf_a, leaf_b) in [
            ("obj:1 a leaf", &cache_a, &cache_b, &id1, &l1a, &l1a),
            ("obj:1 b leaf", &cache_a, &cache_b, &id1, &l1b, &l1b),
            ("obj:2 leaf", &cache_a, &cache_b, &id2, &l2, &l2),
        ] {
            let got_a = ca.get(id).unwrap();
            let got_b = cb.get(id).unwrap();
            assert_eq!(
                got_a.canonical, got_b.canonical,
                "{desc}: canonical mismatch"
            );
            assert!(
                got_a.leaves.contains(leaf_a),
                "{desc}: single-put missing leaf"
            );
            assert!(got_b.leaves.contains(leaf_b), "{desc}: batch missing leaf");
            assert_eq!(
                ca.id_of(leaf_a.as_bytes()).as_deref(),
                cb.id_of(leaf_b.as_bytes()).as_deref(),
                "{desc}: id_of mismatch"
            );
        }

        // obj:3 should be absent in both (neither was written).
        assert!(cache_a.get(&id3).is_none());
        assert!(cache_b.get(&id3).is_none());
    }
}
