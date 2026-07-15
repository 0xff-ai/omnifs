//! Object cache: durable, global, ObjectId-keyed canonical bytes.
//!
//! Backed by a fjall [`Database`]. Each mount owns its own pair of keyspaces,
//! so keys carry no mount prefix:
//! - `objects.{mount}`: `{id}` → postcard of [`ObjectRecord`]
//! - `view.{mount}`:    `{full-path}` → `ObjectId` bytes (objects by view path)
//!
//! Mount isolation is structural (separate LSM-trees), not a key-prefix
//! convention, so there is no in-key mount separator. The per-mount generation
//! fence lives in `MountResources`.
//!
//! Writes are not fsynced per commit: this backs a read-through cache, so any
//! writes lost in a crash are simply refetched from upstream on the next read.
//! We rely on fjall's eventual durability (background memtable flush + journal
//! recovery) rather than forcing `persist(SyncAll)` on the write path.

use anyhow::Result;
use fjall::{Config, Database, Keyspace, KeyspaceCreateOptions};
use std::collections::BTreeMap;
use std::path::Path as StdPath;

/// On-disk schema version for `ObjectRecord`. Bump on layout change.
pub const SCHEMA: u8 = 2;

/// Stored canonical bytes for one object.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StoredObject {
    pub bytes: Vec<u8>,
    pub validator: Option<String>,
}

/// One object row stored in the database. `leaves` are this mount's unscoped
/// view-leaf paths; the caller re-scopes them for view-cache eviction.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ObjectRecord {
    pub schema: u8,
    pub canonical: Option<StoredObject>,
    pub leaves: Vec<String>,
}

impl ObjectRecord {
    fn new(canonical: Option<StoredObject>, leaves: Vec<String>) -> Self {
        Self {
            schema: SCHEMA,
            canonical,
            leaves,
        }
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        let record: Self = postcard::from_bytes(bytes).ok()?;
        (record.schema == SCHEMA).then_some(record)
    }
}

/// One entry in a `MountObjects::store_batch` call. Fence checks and view
/// evictions are the caller's responsibility; this type carries pre-validated
/// data with a raw (unscoped) object id.
pub struct StoreBatchEntry {
    pub id: Vec<u8>,
    pub canonical: StoredObject,
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
        let db = Database::open(Config::new(path)).map_err(|error| match error {
            // Fjall holds a single-writer lock on the database directory. A
            // bare `FjallError: Locked` is opaque; name the real cause so the
            // operator can act instead of reading it as data corruption.
            fjall::Error::Locked => anyhow::anyhow!(
                "object cache at {} is locked: another omnifs daemon is already \
                 using this home. Stop it with `omnifs down`, or kill a stale \
                 `omnifs daemon` process, then retry.",
                path.display()
            ),
            other => anyhow::Error::new(other)
                .context(format!("open object cache at {}", path.display())),
        })?;
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
    /// Index-only upsert for preload fs-writes. Stored bytes beat preload: an
    /// existing `Some` canonical is never clobbered to `None`.
    pub fn store_index_only(&self, id: &[u8], new_leaves: &[String]) -> Result<()> {
        let existing = self.get(id);
        let (canonical, base_leaves) = match existing {
            Some(obj) => (obj.canonical, obj.leaves),
            None => (None, Vec::new()),
        };
        let merged_leaves = merge_leaves(&base_leaves, new_leaves);
        let stored = ObjectRecord::new(canonical, merged_leaves);
        self.commit_object(id, &stored, new_leaves)
    }

    /// Batch canonical store: commits all entries in ONE fjall write batch.
    ///
    /// Fence checks and prior-leaf view evictions are the caller's
    /// responsibility (done before this call so they can be rejected
    /// individually without aborting the batch). Each entry reads existing
    /// leaves, merges, serializes, then all writes land in one atomic batch.
    pub fn store_batch(&self, entries: &[StoreBatchEntry]) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }

        // Group by object id before reading durable state. The last canonical
        // wins while every alias from every entry is retained.
        let mut grouped = BTreeMap::<Vec<u8>, (StoredObject, Vec<String>)>::new();
        for entry in entries {
            let group = grouped
                .entry(entry.id.clone())
                .or_insert_with(|| (entry.canonical.clone(), Vec::new()));
            group.0 = entry.canonical.clone();
            group.1.extend(entry.new_leaves.iter().cloned());
        }

        // Read phase: each object row is decoded once before the durable batch.
        let prepared: Vec<(Vec<u8>, Vec<String>, Vec<u8>)> = grouped
            .into_iter()
            .map(|(id, (canonical, new_leaves))| {
                let prior_leaves = self.leaves_of(&id);
                let merged_leaves = merge_leaves(&prior_leaves, &new_leaves);
                let stored = ObjectRecord::new(Some(canonical), merged_leaves);
                let payload = postcard::to_allocvec(&stored)?;
                Ok::<_, postcard::Error>((id, new_leaves, payload))
            })
            .collect::<std::result::Result<_, _>>()?;

        let mut batch = self.db.batch();
        for (id, new_leaves, payload) in &prepared {
            batch.insert(&self.objects, id.as_slice(), payload.as_slice());
            for leaf in new_leaves {
                batch.insert(&self.view, leaf.as_bytes(), id.as_slice());
            }
        }
        batch.commit()?;
        Ok(())
    }

    pub fn get(&self, id: &[u8]) -> Option<ObjectRecord> {
        let value = self.objects.get(id).ok()??;
        ObjectRecord::decode(&value)
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
    pub fn evict_object(&self, id: &[u8], mut view_evict: impl FnMut(&str)) -> Result<()> {
        let leaves = self.leaves_of(id);
        for leaf in &leaves {
            view_evict(leaf);
        }

        let mut batch = self.db.batch();
        batch.remove(&self.objects, id);
        for leaf in &leaves {
            batch.remove(&self.view, leaf.as_bytes());
        }
        batch.commit()?;
        Ok(())
    }

    fn commit_object(&self, id: &[u8], stored: &ObjectRecord, new_leaves: &[String]) -> Result<()> {
        let payload = postcard::to_allocvec(stored)?;

        let mut batch = self.db.batch();
        batch.insert(&self.objects, id, payload.as_slice());
        for leaf in new_leaves {
            batch.insert(&self.view, leaf.as_bytes(), id);
        }
        batch.commit()?;
        Ok(())
    }
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

    /// A second open while the first holds fjall's single-writer lock reports
    /// the real cause and the fix, not a bare `FjallError: Locked`.
    #[test]
    fn locked_cache_names_the_cause_and_the_fix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("object");
        let _first = Cache::open(&path).expect("first open acquires the lock");
        let Err(error) = Cache::open(&path) else {
            panic!("second open must fail while the first holds the lock");
        };
        let rendered = format!("{error:#}");
        assert!(rendered.contains("is locked"), "got: {rendered}");
        assert!(
            rendered.contains("omnifs down"),
            "should point at the fix: {rendered}"
        );
    }

    const OBJ: &[u8] = b"issue:42";

    #[test]
    fn canonical_beats_preload() {
        let (_dir, cache) = open_cache();
        let l1 = "/issues/open/42/item.json".to_string();
        let c = StoredObject {
            bytes: b"data".to_vec(),
            validator: Some("v1".to_string()),
        };
        cache.store_batch(&[StoreBatchEntry {
            id: OBJ.to_vec(),
            canonical: c,
            new_leaves: vec![l1.clone()],
        }]);

        let l3 = "/issues/open/42/title".to_string();
        cache.store_index_only(OBJ, &[l3]).unwrap();

        let got = cache.get(OBJ).unwrap();
        assert!(got.canonical.is_some());
        assert_eq!(got.canonical.as_ref().unwrap().bytes, b"data");
        assert!(got.leaves.contains(&l1));
        assert!(got.leaves.contains(&"/issues/open/42/title".to_string()));
    }

    #[test]
    fn overwrite_unions_aliases_keeps_index() {
        let (_dir, cache) = open_cache();
        let l1 = "/p/L1".to_string();
        let l2 = "/p/L2".to_string();

        cache.store_batch(&[StoreBatchEntry {
            id: OBJ.to_vec(),
            canonical: StoredObject {
                bytes: b"v1".to_vec(),
                validator: None,
            },
            new_leaves: vec![l1.clone()],
        }]);

        cache.store_batch(&[StoreBatchEntry {
            id: OBJ.to_vec(),
            canonical: StoredObject {
                bytes: b"v2".to_vec(),
                validator: None,
            },
            new_leaves: vec![l2.clone()],
        }]);

        let got = cache.get(OBJ).unwrap();
        assert!(got.leaves.contains(&l1));
        assert!(got.leaves.contains(&l2));
        assert_eq!(cache.id_of(l1.as_bytes()).as_deref(), Some(OBJ));
        assert_eq!(got.canonical.unwrap().bytes, b"v2");
    }

    #[test]
    fn evict_object_removes_object_and_paths() {
        let (_dir, cache) = open_cache();
        let leaf = "/a/leaf".to_string();
        cache.store_batch(&[StoreBatchEntry {
            id: OBJ.to_vec(),
            canonical: StoredObject {
                bytes: b"data".to_vec(),
                validator: None,
            },
            new_leaves: vec![leaf.clone()],
        }]);

        cache.evict_object(OBJ, |_| {});

        assert!(cache.get(OBJ).is_none());
        assert!(cache.id_of(leaf.as_bytes()).is_none());
    }

    #[test]
    fn id_of_exact_lookup() {
        let (_dir, cache) = open_cache();
        let p1 = "/issues/42/item.md";
        let p2 = "/issues/42/title";
        cache.store_batch(&[StoreBatchEntry {
            id: OBJ.to_vec(),
            canonical: StoredObject {
                bytes: b"data".to_vec(),
                validator: None,
            },
            new_leaves: vec![p1.to_string(), p2.to_string()],
        }]);

        assert_eq!(cache.id_of(p1.as_bytes()).as_deref(), Some(OBJ));
        assert_eq!(cache.id_of(p2.as_bytes()).as_deref(), Some(OBJ));
        assert!(cache.id_of("/issues/42/other".as_bytes()).is_none());
    }

    /// Batch-put of N objects yields identical observable state (`get`/`id_of`/`leaves_of`)
    /// to N one-entry batches, including a mixed case with one fence-rejected entry.
    #[test]
    fn store_batch_equivalent_to_one_entry_batches() {
        let (_dir_a, cache_a) = open_cache();
        let (_dir_b, cache_b) = open_cache();

        let id1 = b"obj:1" as &[u8];
        let id2 = b"obj:2" as &[u8];
        let id3 = b"obj:3" as &[u8]; // "fence-rejected" by the caller

        let l1a = "/issues/1/item.json".to_string();
        let l1b = "/issues/all/1/item.json".to_string();
        let l2 = "/issues/2/item.json".to_string();

        // One-entry batch baseline: obj:3 is intentionally omitted (simulates rejection).
        cache_a.store_batch(&[StoreBatchEntry {
            id: id1.to_vec(),
            canonical: StoredObject {
                bytes: b"payload1".to_vec(),
                validator: Some("v1".to_string()),
            },
            new_leaves: vec![l1a.clone(), l1b.clone()],
        }]);
        cache_a.store_batch(&[StoreBatchEntry {
            id: id2.to_vec(),
            canonical: StoredObject {
                bytes: b"payload2".to_vec(),
                validator: None,
            },
            new_leaves: vec![l2.clone()],
        }]);

        // Batch-put equivalent (obj:3 excluded, same as single-put baseline).
        cache_b.store_batch(&[
            StoreBatchEntry {
                id: id1.to_vec(),
                canonical: StoredObject {
                    bytes: b"payload1".to_vec(),
                    validator: Some("v1".to_string()),
                },
                new_leaves: vec![l1a.clone(), l1b.clone()],
            },
            StoreBatchEntry {
                id: id2.to_vec(),
                canonical: StoredObject {
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
