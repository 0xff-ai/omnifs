//! Object cache : durable, global, ObjectId-keyed canonical bytes.
//!
//! Backed by a fjall keyspace with two partitions (byte keys throughout):
//! - `objects`: `mount\x1f{id}` → postcard of `StoredObject`
//! - `paths`:   `mount\x1f{full-path}` → scoped `ObjectId` bytes
//!
//! The cache is mount-agnostic; all keys are pre-scoped by the caller
//! (`Store::scoped_id` / `Store::scoped_path_bytes`). The per-mount generation
//! fence lives in `Store`.

use anyhow::Result;
use fjall::{Config, Database, Keyspace, KeyspaceCreateOptions};
use std::path::Path;

/// On-disk schema version for `StoredObject`. Bump on layout change.
pub const SCHEMA: u8 = 1;

const OBJECTS_KEYSPACE: &str = "objects";
const PATHS_KEYSPACE: &str = "paths";

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

/// Global, durable object-id cache. One instance per process; mount isolation
/// is enforced by the `mount\x1f` key prefix injected by `Store`.
///
/// Writes are not fsynced per commit: this backs a read-through cache, so any
/// writes lost in a crash are simply refetched from upstream on the next read.
/// We rely on fjall's eventual durability (background memtable flush + journal
/// rotation) rather than forcing `persist(SyncAll)` on the write path.
pub struct Cache {
    db: Database,
    objects: Keyspace,
    paths: Keyspace,
}

impl Cache {
    /// Open the durable object database at `path`, creating it if absent.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let db = Database::open(Config::new(path))?;
        let objects = db.keyspace(OBJECTS_KEYSPACE, KeyspaceCreateOptions::default)?;
        let paths = db.keyspace(PATHS_KEYSPACE, KeyspaceCreateOptions::default)?;
        Ok(Self { db, objects, paths })
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

    pub fn get(&self, scoped_id: &[u8]) -> Option<StoredObject> {
        let value = self.objects.get(scoped_id).ok()??;
        decode_object(&value)
    }

    /// Forward index: scoped full path → scoped `ObjectId` bytes.
    pub fn id_of(&self, scoped_path: &[u8]) -> Option<Vec<u8>> {
        let value = self.paths.get(scoped_path).ok()??;
        Some(value.to_vec())
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

        let result = (|| -> Result<()> {
            let mut batch = self.db.batch();
            batch.remove(&self.objects, scoped_id);
            for leaf in &leaves {
                batch.remove(&self.paths, leaf.as_bytes());
            }
            batch.commit()?;
            Ok(())
        })();

        if let Err(e) = result {
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

        let result = (|| -> Result<()> {
            let payload = postcard::to_allocvec(&obj).map_err(anyhow::Error::from)?;
            self.objects.insert(scoped_id, payload.as_slice())?;
            Ok(())
        })();

        if let Err(e) = result {
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

        let result = (|| -> Result<()> {
            let mut batch = self.db.batch();
            batch.insert(&self.objects, scoped_id, payload.as_slice());
            for leaf in new_leaves {
                batch.insert(&self.paths, leaf.as_bytes(), scoped_id);
            }
            batch.commit()?;
            Ok(())
        })();

        match result {
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
    let mut merged = existing.to_vec();
    for leaf in new_leaves {
        if !merged.contains(leaf) {
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
        let cache = Cache::open(&dir.path().join("object")).unwrap();
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
}
