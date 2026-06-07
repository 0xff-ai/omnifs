//! Host-owned cache materialization.
//!
//! `Materializer` owns the translation from provider wire types (`wit_types`)
//! into cache storage primitives. The cache (`omnifs_cache::Store`) is pure
//! byte storage; it knows nothing about the wire protocol. All
//! wire→storage translation lives here.

use std::collections::BTreeMap;

use omnifs_cache::{BatchRecord, Record, RecordKind, Store};
use omnifs_core::path::Path;
use omnifs_core::view::{DirentRecord, DirentsPayload, EntryMeta, Stability};
use tracing::{debug, warn};

use crate::clock::MUTABLE_TTL_MILLIS;
use crate::object_id::ObjectId;
use crate::pagination;
use crate::projection::{self, push_projected_entry, push_projected_file_content};
use crate::wit_protocol::{entry_meta_from_kind, file_attrs_from_file_out, stability_from_wit};
use omnifs_wit::provider::types as wit_types;

/// Host-facing result of a `lookup-child` after provider wire data has been
/// materialized into host cache structures.
#[derive(Debug, Clone)]
pub enum LookupOutcome {
    Entry(LookupEntry),
    Subtree(u64),
    NotFound,
}

/// Materialized lookup entry consumed by runtime adapters.
#[derive(Debug, Clone)]
pub struct LookupEntry {
    path: Path,
    meta: EntryMeta,
}

impl LookupEntry {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn meta(&self) -> &EntryMeta {
        &self.meta
    }
}

/// Translates provider wire effects and browse results into cache storage
/// calls. Holds a reference to the per-mount `Store` for the duration of
/// one materialization call.
pub struct Materializer<'a> {
    store: &'a Store,
}

impl<'a> Materializer<'a> {
    pub fn new(store: &'a Store) -> Self {
        Self { store }
    }

    /// Apply the cache-bearing effects carried by a provider terminal.
    ///
    /// Returns `(invalidated_prefixes, invalidated_paths)`. FUSE adapters
    /// translate those into kernel cache-invalidation notifications.
    #[allow(clippy::too_many_lines)]
    pub fn apply(
        &self,
        effects: &wit_types::Effects,
        op_gen: u64,
        now_millis: u64,
    ) -> (Vec<String>, Vec<String>) {
        for store in &effects.canonical {
            let id = ObjectId::from_wit(&store.id);
            if self.rejects_conflicting_id(&store.view_leaves, &id) {
                continue;
            }
            self.store.put_canonical(
                id.as_bytes(),
                store.bytes.clone(),
                store.validator.clone(),
                &store.view_leaves,
                op_gen,
            );
        }

        let mut batch: Vec<BatchRecord> = Vec::new();
        let mut dirs: BTreeMap<Path, bool> = BTreeMap::new();
        let mut children: BTreeMap<Path, BTreeMap<String, DirentRecord>> = BTreeMap::new();

        for write in &effects.fs {
            let Ok(write_path) = Path::parse(&write.path) else {
                warn!(
                    path = write.path.as_str(),
                    "fs-effect projection yielded an invalid protocol path; skipping"
                );
                continue;
            };

            if let Some((_, name)) = split_projected_path(&write.path)
                && pagination::is_reserved_provider_leaf(&name)
            {
                warn!(
                    path = write.path.as_str(),
                    "fs-effect projection yielded a reserved '@'-prefixed leaf; skipping"
                );
                continue;
            }

            let mut admit_view = true;
            if let Some(id) = &write.id {
                let oid = ObjectId::from_wit(id);
                if self.rejects_conflicting_id(std::slice::from_ref(&write.path), &oid) {
                    continue;
                }
                self.store.put_index_only(
                    oid.as_bytes(),
                    std::slice::from_ref(&write.path),
                    op_gen,
                );
                if self.store.write_fenced(&write_path, op_gen) {
                    admit_view = false;
                }
            }

            if let wit_types::FsKind::Directory(listing_exhaustive) = &write.kind {
                let existing = dirs.entry(write_path.clone()).or_insert(false);
                *existing = *existing || *listing_exhaustive;
            }

            if admit_view {
                if let Some((parent, name)) = split_projected_path(&write.path) {
                    let meta = match &write.kind {
                        wit_types::FsKind::Directory(_) => EntryMeta::directory(),
                        wit_types::FsKind::File(file) => {
                            EntryMeta::file(file_attrs_from_file_out(file))
                        },
                    };
                    children
                        .entry(parent)
                        .or_default()
                        .insert(name.clone(), DirentRecord { name, meta });
                }

                let mut leaf_records = Vec::new();
                match &write.kind {
                    wit_types::FsKind::Directory(_) => {
                        push_projected_entry(
                            &mut leaf_records,
                            &write.path,
                            &wit_types::EntryKind::Directory,
                        );
                    },
                    wit_types::FsKind::File(file) => {
                        push_projected_entry(
                            &mut leaf_records,
                            &write.path,
                            &wit_types::EntryKind::File(file.clone()),
                        );
                    },
                }
                if let wit_types::FsKind::File(file) = &write.kind {
                    push_projected_file_content(&mut leaf_records, &write.path, file);
                    if write.id.is_some() {
                        let stability = stability_from_wit(file.attrs.stability);
                        let expires_at = freshness_expiry(stability, now_millis);
                        self.store
                            .cache_view_leaf(&write_path, &leaf_records, expires_at, op_gen);
                    } else {
                        batch.extend(leaf_records);
                    }
                } else {
                    batch.extend(leaf_records);
                }
            }
        }

        for (dir, listing_exhaustive) in dirs {
            if let Some(new_children) = children.remove(&dir) {
                self.store
                    .update_metadata_record(&dir, RecordKind::Dirents, None, |existing| {
                        let existing = existing
                            .and_then(|record| DirentsPayload::deserialize(&record.payload));
                        let payload =
                            DirentsPayload::merged(existing, new_children, listing_exhaustive);
                        payload
                            .serialize()
                            .map(|payload| Record::new(RecordKind::Dirents, payload))
                    });
            }
        }

        if !batch.is_empty() {
            debug!(
                target: "omnifs_cache",
                kind = "project",
                count = batch.len(),
                "applying fs-write effects"
            );
            self.store.cache_put_batch(&batch);
        }

        let mut invalidated_prefixes = Vec::new();
        let mut invalidated_paths = Vec::new();
        for invalidation in &effects.invalidations {
            match invalidation {
                wit_types::Invalidation::Object(id) => {
                    let oid = ObjectId::from_wit(id);
                    let paths = self.store.paths_for_id(oid.as_bytes());
                    self.store.delete_object(oid.as_bytes());
                    invalidated_paths.extend(paths);
                },
                wit_types::Invalidation::Listing(wit_types::PathOrPrefix::Path(p)) => {
                    if let Ok(path) = Path::parse(p) {
                        self.store.delete_listing_path(&path);
                    }
                    invalidated_paths.push(p.clone());
                },
                wit_types::Invalidation::Listing(wit_types::PathOrPrefix::Prefix(p)) => {
                    if let Ok(prefix) = Path::parse(p) {
                        self.store.delete_listing_prefix(&prefix);
                    }
                    invalidated_prefixes.push(p.clone());
                },
            }
        }

        (invalidated_prefixes, invalidated_paths)
    }

    /// Materialize a `lookup-child` result into cache storage and return the
    /// host-facing browse outcome for runtime adapters.
    pub fn lookup(
        &self,
        parent_path: &Path,
        child_path: &Path,
        result: wit_types::LookupChildResult,
        op_gen: u64,
        now_millis: u64,
    ) -> LookupOutcome {
        match result {
            wit_types::LookupChildResult::Entry(entry) => {
                self.apply_lookup_projection(parent_path, &entry, op_gen);
                LookupOutcome::Entry(LookupEntry {
                    path: child_path.clone(),
                    meta: entry_meta_from_kind(&entry.target.kind),
                })
            },
            wit_types::LookupChildResult::Subtree(tree_ref) => LookupOutcome::Subtree(tree_ref),
            wit_types::LookupChildResult::NotFound(maybe_id) => {
                self.apply_negative_lookup(child_path, maybe_id.as_ref(), op_gen, now_millis);
                LookupOutcome::NotFound
            },
        }
    }

    /// Cache the result of a `lookup-child` call, including any sibling hints
    /// the provider returned alongside the primary target.
    pub fn apply_lookup_projection(
        &self,
        parent_path: &Path,
        entry: &wit_types::LookupEntry,
        op_gen: u64,
    ) {
        projection::apply_lookup_projection(self.store, parent_path, entry);
        self.index_entry_ids(parent_path, entry, op_gen);
    }

    fn apply_negative_lookup(
        &self,
        child_path: &Path,
        maybe_id: Option<&wit_types::LogicalId>,
        op_gen: u64,
        now_millis: u64,
    ) {
        let id_bytes = maybe_id.map(|id| ObjectId::from_wit(id).as_bytes().to_vec());
        self.store.put_negative(
            child_path,
            id_bytes.as_deref(),
            op_gen,
            MUTABLE_TTL_MILLIS,
            now_millis,
        );
    }

    /// Cache the authoritative listing from a `list-children` response.
    pub fn apply_listing_projection(
        &self,
        path: &Path,
        listing: &wit_types::DirListing,
        op_gen: u64,
    ) {
        projection::apply_listing_projection(self.store, path, listing);
        for entry in &listing.entries {
            self.index_single_entry_id(path, entry, op_gen);
        }
    }

    /// Cache a continuation page from a paged `list-children` response.
    pub fn apply_continuation_projection(
        &self,
        path: &Path,
        entries: &[wit_types::DirEntry],
        op_gen: u64,
    ) {
        projection::apply_continuation_projection(self.store, path, entries);
        for entry in entries {
            self.index_single_entry_id(path, entry, op_gen);
        }
    }

    fn index_entry_ids(&self, parent_path: &Path, entry: &wit_types::LookupEntry, op_gen: u64) {
        self.index_single_entry_id(parent_path, &entry.target, op_gen);
        for sibling in &entry.siblings {
            self.index_single_entry_id(parent_path, sibling, op_gen);
        }
    }

    fn index_single_entry_id(&self, parent_path: &Path, entry: &wit_types::DirEntry, op_gen: u64) {
        let Some(id) = &entry.id else {
            return;
        };
        let Ok(entry_path) = parent_path.join(&entry.name) else {
            warn!(
                parent = parent_path.as_str(),
                name = entry.name.as_str(),
                "provider entry id used an invalid protocol path segment; skipping id index"
            );
            return;
        };
        let oid = ObjectId::from_wit(id);
        let entry_path_string = entry_path.as_str().to_string();
        if self.rejects_conflicting_id(std::slice::from_ref(&entry_path_string), &oid) {
            return;
        }
        self.store
            .put_index_only(oid.as_bytes(), &[entry_path_string], op_gen);
    }

    /// True if any of `paths` is already indexed to an id != `id` (a provider bug).
    fn rejects_conflicting_id(&self, paths: &[String], id: &ObjectId) -> bool {
        paths.iter().any(|p| {
            let Ok(path) = Path::parse(p) else {
                tracing::warn!(
                    path = p.as_str(),
                    "effect maps an invalid protocol path; rejecting"
                );
                return true;
            };
            self.store.id_of_path(&path).is_some_and(|existing| {
                if existing.as_slice() == id.as_bytes() {
                    false
                } else {
                    tracing::warn!(
                        path = p.as_str(),
                        "effect maps an indexed path to a different id; rejecting"
                    );
                    true
                }
            })
        })
    }
}

fn split_projected_path(path: &str) -> Option<(Path, String)> {
    let path = Path::parse(path).ok()?;
    let (parent, name) = path.parent_and_name()?;
    Some((parent, name.to_string()))
}

fn freshness_expiry(stability: Stability, now_millis: u64) -> Option<u64> {
    match stability {
        Stability::Immutable => None,
        Stability::Mutable => Some(now_millis.saturating_add(MUTABLE_TTL_MILLIS)),
        Stability::Volatile => Some(now_millis),
    }
}
