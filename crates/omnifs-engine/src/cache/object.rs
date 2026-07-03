//! Object cache: durable, global, ObjectId-keyed canonical bytes.
//!
//! Backed by a fjall [`Database`]. Each mount owns its own pair of keyspaces,
//! so keys carry no mount prefix:
//! - `objects.{mount}`: `{id}` → postcard of [`StoredObject`]
//! - `view.{mount}`:    `{full-path}` → `ObjectId` bytes (objects by view path)
//!
//! Mount isolation is structural (separate LSM-trees), not a key-prefix
//! convention, so there is no in-key mount separator. The per-mount generation
//! fence lives in `Store`.
//!
//! Writes are not fsynced per commit: this backs a read-through cache, so any
//! writes lost in a crash are simply refetched from upstream on the next read.
//! We rely on fjall's eventual durability (background memtable flush + journal
//! recovery) rather than forcing `persist(SyncAll)` on the write path.

use anyhow::Result;
use fjall::{Config, Database, Keyspace, KeyspaceCreateOptions};
use std::path::Path as StdPath;

/// On-disk schema version for `StoredObject`. Bump on layout change.
pub const SCHEMA: u8 = 1;

/// Canonical bytes for one object.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Canonical {
    pub bytes: Vec<u8>,
    pub validator: Option<String>,
}

/// One object row stored in the database. `leaves` are this mount's unscoped
/// view-leaf paths; the caller re-scopes them for view-cache eviction.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StoredObject {
    pub schema: u8,
    pub id: Vec<u8>,
    pub canonical: Option<Canonical>,
    pub leaves: Vec<String>,
}

impl StoredObject {
    fn new(id: &[u8], canonical: Option<Canonical>, leaves: Vec<String>) -> Self {
        Self {
            schema: SCHEMA,
            id: id.to_vec(),
            canonical,
            leaves,
        }
    }
}

/// One entry in a `MountObjects::store_batch` call. Fence checks and view
/// evictions are the caller's responsibility; this type carries pre-validated
/// data with a raw (unscoped) object id.
pub struct StoreBatchEntry {
    pub id: Vec<u8>,
    pub canonical: Canonical,
    pub new_leaves: Vec<String>,
}

/// Global, durable object database. One instance per process; per-mount
/// keyspaces are obtained via [`Cache::mount`].
pub struct Cache {
    db: Database,
}

impl Cache {
    /// Open the durable object database at `path` (a directory), creating it if
    /// absent.
    pub fn open(path: &StdPath) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let db = Database::open(Config::new(path))?;
        Ok(Self { db })
    }

    /// Get-or-create this mount's object keyspaces. Mount names are validated
    /// `[a-z0-9-]{1,32}` (see `omnifs_core` mount validation), a subset of
    /// fjall's legal keyspace charset, so they embed directly in the name.
    pub fn mount(&self, mount: &str) -> Result<MountObjects> {
        let objects = self
            .db
            .keyspace(&format!("objects.{mount}"), KeyspaceCreateOptions::default)?;
        let view = self
            .db
            .keyspace(&format!("view.{mount}"), KeyspaceCreateOptions::default)?;
        Ok(MountObjects {
            db: self.db.clone(),
            objects,
            view,
        })
    }
}

/// One mount's object keyspaces. All keys are raw (unscoped): the mount lives
/// in the keyspace name, not the key.
pub struct MountObjects {
    db: Database,
    objects: Keyspace,
    /// View-path index: full view path → object id.
    view: Keyspace,
}

impl MountObjects {
    /// UPSERT: union `new_leaves`, set canonical, keep existing VIEW rows, add
    /// rows for new leaves, evict rendered view bytes for current leaves first.
    pub fn store(
        &self,
        id: &[u8],
        canonical: Canonical,
        new_leaves: &[String],
        mut view_evict: impl FnMut(&str),
    ) -> bool {
        let prior_leaves = self.leaves_of(id);
        for leaf in &prior_leaves {
            view_evict(leaf);
        }

        let merged_leaves = merge_leaves(&prior_leaves, new_leaves);
        let stored = StoredObject::new(id, Some(canonical), merged_leaves);

        self.commit_object(id, &stored, new_leaves)
    }

    /// Index-only upsert for preload fs-writes. Canonical-beats-preload: an
    /// existing `Some` canonical is never clobbered to `None`.
    pub fn store_index_only(&self, id: &[u8], new_leaves: &[String]) -> bool {
        let existing = self.get(id);
        let (canonical, base_leaves) = match existing {
            Some(obj) => (obj.canonical, obj.leaves),
            None => (None, Vec::new()),
        };
        let merged_leaves = merge_leaves(&base_leaves, new_leaves);
        let stored = StoredObject::new(id, canonical, merged_leaves);
        self.commit_object(id, &stored, new_leaves)
    }

    /// Batch canonical store: commits all entries in ONE fjall write batch.
    ///
    /// Fence checks and prior-leaf view evictions are the caller's
    /// responsibility (done before this call so they can be rejected
    /// individually without aborting the batch). Each entry reads existing
    /// leaves, merges, serializes, then all writes land in one atomic batch.
    pub fn store_batch(&self, entries: &[StoreBatchEntry]) {
        if entries.is_empty() {
            return;
        }

        // Read phase: collect prior leaves + build StoredObject for each entry.
        let prepared: Vec<(&StoreBatchEntry, Vec<u8>)> = entries
            .iter()
            .filter_map(|e| {
                let prior_leaves = self.leaves_of(&e.id);
                let merged_leaves = merge_leaves(&prior_leaves, &e.new_leaves);
                let stored = StoredObject::new(&e.id, Some(e.canonical.clone()), merged_leaves);
                match postcard::to_allocvec(&stored) {
                    Ok(payload) => Some((e, payload)),
                    Err(err) => {
                        tracing::warn!(
                            error = %err,
                            "object cache: batch serialize failed; skipping entry"
                        );
                        None
                    },
                }
            })
            .collect();

        if prepared.is_empty() {
            return;
        }

        let mut batch = self.db.batch();
        for (entry, payload) in &prepared {
            batch.insert(&self.objects, entry.id.as_slice(), payload.as_slice());
            for leaf in &entry.new_leaves {
                batch.insert(&self.view, leaf.as_bytes(), entry.id.as_slice());
            }
        }
        if let Err(e) = batch.commit() {
            tracing::warn!(error = %e, "object cache: batch write failed");
        }
    }

    pub fn get(&self, id: &[u8]) -> Option<StoredObject> {
        let value = self.objects.get(id).ok()??;
        decode_object(&value)
    }

    /// Forward index: full path bytes → `ObjectId` bytes.
    pub fn id_of(&self, path: &[u8]) -> Option<Vec<u8>> {
        let value = self.view.get(path).ok()??;
        Some(value.to_vec())
    }

    /// Alias set for `id` (unscoped view-leaf paths). Empty when absent.
    pub fn leaves_of(&self, id: &[u8]) -> Vec<String> {
        self.get(id).map(|obj| obj.leaves).unwrap_or_default()
    }

    /// Full eviction: OBJECTS row, every VIEW row in the alias set, and view leaves.
    pub fn evict_object(&self, id: &[u8], mut view_evict: impl FnMut(&str)) {
        let leaves = self.leaves_of(id);
        for leaf in &leaves {
            view_evict(leaf);
        }

        let mut batch = self.db.batch();
        batch.remove(&self.objects, id);
        for leaf in &leaves {
            batch.remove(&self.view, leaf.as_bytes());
        }
        if let Err(e) = batch.commit() {
            tracing::warn!(error = %e, "object cache: evict_object failed");
        }
    }

    /// Capacity eviction: drop canonical bytes + validator and evict rendered view
    /// leaves, but keep the OBJECTS row (canonical=None) and all VIEW rows.
    pub fn capacity_evict(&self, id: &[u8], mut view_evict: impl FnMut(&str)) {
        let Some(mut obj) = self.get(id) else {
            return;
        };
        for leaf in &obj.leaves {
            view_evict(leaf);
        }
        obj.canonical = None;

        let payload = match postcard::to_allocvec(&obj) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "object cache: capacity_evict serialize failed");
                return;
            },
        };
        if let Err(e) = self.objects.insert(id, payload.as_slice()) {
            tracing::warn!(error = %e, "object cache: capacity_evict failed");
        }
    }

    fn commit_object(&self, id: &[u8], stored: &StoredObject, new_leaves: &[String]) -> bool {
        let payload = match postcard::to_allocvec(stored) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "object cache: serialize failed");
                return false;
            },
        };

        let mut batch = self.db.batch();
        batch.insert(&self.objects, id, payload.as_slice());
        for leaf in new_leaves {
            batch.insert(&self.view, leaf.as_bytes(), id);
        }
        match batch.commit() {
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

    fn open_cache() -> (tempfile::TempDir, MountObjects) {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(&dir.path().join("object")).unwrap();
        let mount = cache.mount("m").unwrap();
        (dir, mount)
    }

    const OBJ: &[u8] = b"issue:42";

    #[test]
    fn canonical_beats_preload() {
        let (_dir, cache) = open_cache();
        let l1 = "/issues/open/42/item.json".to_string();
        let c = Canonical {
            bytes: b"data".to_vec(),
            validator: Some("v1".to_string()),
        };
        assert!(cache.store(OBJ, c, std::slice::from_ref(&l1), |_| {}));

        let l3 = "/issues/open/42/title".to_string();
        assert!(cache.store_index_only(OBJ, &[l3]));

        let got = cache.get(OBJ).unwrap();
        assert!(got.canonical.is_some());
        assert_eq!(got.canonical.as_ref().unwrap().bytes, b"data");
        assert!(got.leaves.contains(&l1));
        assert!(got.leaves.contains(&"/issues/open/42/title".to_string()));
    }

    #[test]
    fn overwrite_unions_aliases_and_evicts_prior_views() {
        let (_dir, cache) = open_cache();
        let l1 = "/p/L1".to_string();
        let l2 = "/p/L2".to_string();

        cache.store(
            OBJ,
            Canonical {
                bytes: b"v1".to_vec(),
                validator: None,
            },
            std::slice::from_ref(&l1),
            |_| {},
        );

        let mut evicted = Vec::new();
        cache.store(
            OBJ,
            Canonical {
                bytes: b"v2".to_vec(),
                validator: None,
            },
            std::slice::from_ref(&l2),
            |leaf| evicted.push(leaf.to_string()),
        );

        let got = cache.get(OBJ).unwrap();
        assert!(got.leaves.contains(&l1));
        assert!(got.leaves.contains(&l2));
        assert_eq!(cache.id_of(l1.as_bytes()).as_deref(), Some(OBJ));
        assert_eq!(got.canonical.unwrap().bytes, b"v2");

        let mut evicted_sorted = evicted;
        evicted_sorted.sort();
        assert_eq!(evicted_sorted, vec![l1]);
    }

    #[test]
    fn capacity_evict_keeps_index_drops_validator() {
        let (_dir, cache) = open_cache();
        let leaf = "/a/leaf".to_string();
        cache.store(
            OBJ,
            Canonical {
                bytes: b"data".to_vec(),
                validator: Some("etag".to_string()),
            },
            std::slice::from_ref(&leaf),
            |_| {},
        );

        cache.capacity_evict(OBJ, |_| {});

        let got = cache.get(OBJ).unwrap();
        assert!(got.canonical.is_none());
        assert_eq!(cache.id_of(leaf.as_bytes()).as_deref(), Some(OBJ));
    }

    #[test]
    fn evict_object_removes_object_and_paths() {
        let (_dir, cache) = open_cache();
        let leaf = "/a/leaf".to_string();
        cache.store(
            OBJ,
            Canonical {
                bytes: b"data".to_vec(),
                validator: None,
            },
            std::slice::from_ref(&leaf),
            |_| {},
        );

        cache.evict_object(OBJ, |_| {});

        assert!(cache.get(OBJ).is_none());
        assert!(cache.id_of(leaf.as_bytes()).is_none());
    }

    #[test]
    fn id_of_exact_lookup() {
        let (_dir, cache) = open_cache();
        let p1 = "/issues/42/item.md";
        let p2 = "/issues/42/title";
        cache.store(
            OBJ,
            Canonical {
                bytes: b"data".to_vec(),
                validator: None,
            },
            &[p1.to_string(), p2.to_string()],
            |_| {},
        );

        assert_eq!(cache.id_of(p1.as_bytes()).as_deref(), Some(OBJ));
        assert_eq!(cache.id_of(p2.as_bytes()).as_deref(), Some(OBJ));
        assert!(cache.id_of("/issues/42/other".as_bytes()).is_none());
    }

    /// Batch-put of N objects yields identical observable state (`get`/`id_of`/`leaves_of`)
    /// to N individual single puts, including a mixed case with one fence-rejected entry.
    #[test]
    fn store_batch_equivalent_to_single_puts() {
        let (_dir_a, cache_a) = open_cache();
        let (_dir_b, cache_b) = open_cache();

        let id1 = b"obj:1" as &[u8];
        let id2 = b"obj:2" as &[u8];
        let id3 = b"obj:3" as &[u8]; // "fence-rejected" by the caller

        let l1a = "/issues/1/item.json".to_string();
        let l1b = "/issues/all/1/item.json".to_string();
        let l2 = "/issues/2/item.json".to_string();

        // Single-put baseline: obj:3 is intentionally omitted (simulates rejection).
        cache_a.store(
            id1,
            Canonical {
                bytes: b"payload1".to_vec(),
                validator: Some("v1".to_string()),
            },
            &[l1a.clone(), l1b.clone()],
            |_| {},
        );
        cache_a.store(
            id2,
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
                id: id1.to_vec(),
                canonical: Canonical {
                    bytes: b"payload1".to_vec(),
                    validator: Some("v1".to_string()),
                },
                new_leaves: vec![l1a.clone(), l1b.clone()],
            },
            StoreBatchEntry {
                id: id2.to_vec(),
                canonical: Canonical {
                    bytes: b"payload2".to_vec(),
                    validator: None,
                },
                new_leaves: vec![l2.clone()],
            },
        ]);

        for (desc, ca, cb, id, leaf) in [
            ("obj:1 a leaf", &cache_a, &cache_b, id1, &l1a),
            ("obj:1 b leaf", &cache_a, &cache_b, id1, &l1b),
            ("obj:2 leaf", &cache_a, &cache_b, id2, &l2),
        ] {
            let got_a = ca.get(id).unwrap();
            let got_b = cb.get(id).unwrap();
            assert_eq!(
                got_a.canonical, got_b.canonical,
                "{desc}: canonical mismatch"
            );
            assert!(
                got_a.leaves.contains(leaf),
                "{desc}: single-put missing leaf"
            );
            assert!(got_b.leaves.contains(leaf), "{desc}: batch missing leaf");
            assert_eq!(
                ca.id_of(leaf.as_bytes()).as_deref(),
                cb.id_of(leaf.as_bytes()).as_deref(),
                "{desc}: id_of mismatch"
            );
        }

        assert!(cache_a.get(id3).is_none());
        assert!(cache_b.get(id3).is_none());
    }
}
