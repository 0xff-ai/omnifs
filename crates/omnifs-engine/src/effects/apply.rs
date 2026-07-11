//! Host-owned cache materialization.
//!
//! `EffectApplier` owns the translation from provider wire types (`wit_types`)
//! into cache storage primitives. The cache (`crate::cache::Store`) is pure
//! byte storage; it knows nothing about the provider component protocol. All
//! wire→storage translation lives here.

use std::collections::BTreeMap;

use crate::cache::{BatchRecord, CanonicalBatchEntry, Record, RecordKind, Store};
use crate::view::{
    AttrPayload, CachedCursor, DirentRecord, DirentsPayload, EntryMeta, FilePayload, LookupPayload,
    Stability,
};
use omnifs_core::path::Path;
use tracing::{debug, warn};

use crate::clock::DYNAMIC_TTL_MILLIS;
use crate::object_id::ObjectId;
use crate::tree::synthetic;
use crate::wit_protocol::{
    cached_cursor_from_wit, entry_meta_from_kind, file_attrs_from_file_out, stability_from_wit,
};
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
pub struct EffectApplier<'a> {
    store: &'a Store,
}

impl<'a> EffectApplier<'a> {
    pub fn new(store: &'a Store) -> Self {
        Self { store }
    }

    /// Apply the cache-bearing effects carried by a provider terminal.
    ///
    /// Returns `(invalidated_prefixes, invalidated_paths)`. FUSE adapters
    /// translate those into kernel cache-invalidation notifications.
    pub fn apply(
        &self,
        effects: &wit_types::Effects,
        op_gen: u64,
        now_millis: u64,
    ) -> (Vec<Path>, Vec<Path>) {
        self.apply_canonical_batch(effects, op_gen);
        let mut fs_effects = self.apply_fs_effects(effects, op_gen, now_millis);
        self.merge_dirents(fs_effects.dirs, &mut fs_effects.children);
        if !fs_effects.batch.is_empty() {
            debug!(
                target: "omnifs_engine_cache",
                kind = "project",
                count = fs_effects.batch.len(),
                "applying fs-write effects"
            );
            self.store.cache_put_batch(&fs_effects.batch);
        }
        self.apply_invalidations(effects)
    }

    fn apply_canonical_batch(&self, effects: &wit_types::Effects, op_gen: u64) {
        // Collect canonical-store effects that pass conflict detection, then
        // write them all in one batch via put_canonical_batch.
        let canonical_batch: Vec<CanonicalBatchEntry> = effects
            .canonical
            .iter()
            .filter_map(|store| {
                let id = ObjectId::from_wit(&store.id);
                let view_leaves = match store
                    .view_leaves
                    .iter()
                    .map(|path| Path::parse(path))
                    .collect::<Result<Vec<_>, _>>()
                {
                    Ok(leaves) => leaves,
                    Err(error) => {
                        warn!(%error, "skipping canonical store: invalid wire path");
                        return None;
                    },
                };
                if self.rejects_conflicting_id(&view_leaves.iter().collect::<Vec<_>>(), &id) {
                    return None;
                }
                Some(CanonicalBatchEntry {
                    id: id.as_bytes().to_vec(),
                    bytes: store.bytes.clone(),
                    validator: store.validator.clone(),
                    view_leaves,
                })
            })
            .collect();
        if !canonical_batch.is_empty() {
            self.store.put_canonical_batch(canonical_batch, op_gen);
        }
    }

    fn apply_fs_effects(
        &self,
        effects: &wit_types::Effects,
        op_gen: u64,
        now_millis: u64,
    ) -> FsEffectRecords {
        let mut records = FsEffectRecords::default();
        for write in &effects.fs {
            let Ok(write_path) = Path::parse(&write.path) else {
                warn!(
                    path = write.path.as_str(),
                    "fs-effect projection yielded an invalid protocol path; skipping"
                );
                continue;
            };

            if let Some((_, name)) = split_projected_path(&write_path)
                && synthetic::is_reserved_provider_leaf(&name)
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
                if self.rejects_conflicting_id(&[&write_path], &oid) {
                    continue;
                }
                self.store.put_index_only(
                    oid.as_bytes(),
                    std::slice::from_ref(&write_path),
                    op_gen,
                );
                if self.store.write_fenced(&write_path, op_gen) {
                    admit_view = false;
                }
            }

            if let wit_types::FsKind::Directory(listing_exhaustive) = &write.kind {
                let existing = records.dirs.entry(write_path.clone()).or_insert(false);
                *existing = *existing || *listing_exhaustive;
            }

            if admit_view {
                let meta = match &write.kind {
                    wit_types::FsKind::Directory(_) => EntryMeta::directory(),
                    wit_types::FsKind::File(file) => {
                        EntryMeta::file(file_attrs_from_file_out(file))
                    },
                };
                if let Some((parent, name)) = split_projected_path(&write_path) {
                    records.children.entry(parent).or_default().insert(
                        name.clone(),
                        DirentRecord {
                            name,
                            meta: meta.clone(),
                        },
                    );
                }

                let mut leaf_records = Vec::new();
                push_projected_entry(&mut leaf_records, &write_path, meta);
                if let wit_types::FsKind::File(file) = &write.kind {
                    push_projected_file_content(&mut leaf_records, &write_path, file);
                    if write.id.is_some() {
                        let stability = stability_from_wit(file.attrs.stability);
                        let expires_at = freshness_expiry(stability, now_millis);
                        self.store
                            .cache_view_leaf(&write_path, &leaf_records, expires_at, op_gen);
                    } else {
                        records.batch.extend(leaf_records);
                    }
                } else {
                    records.batch.extend(leaf_records);
                }
            }
        }
        records
    }

    fn merge_dirents(
        &self,
        dirs: BTreeMap<Path, bool>,
        children: &mut BTreeMap<Path, BTreeMap<String, DirentRecord>>,
    ) {
        for (dir, listing_exhaustive) in dirs {
            if let Some(new_children) = children.remove(&dir) {
                self.store
                    .update_metadata_record(&dir, RecordKind::Dirents, None, |existing| {
                        let existing = existing
                            .and_then(|record| DirentsPayload::deserialize(&record.payload));
                        // The merge reruns on a write-write conflict, so clone
                        // the incoming children rather than moving them.
                        let payload = DirentsPayload::merged(
                            existing,
                            new_children.clone(),
                            listing_exhaustive,
                        );
                        payload
                            .serialize()
                            .map(|payload| Record::new(RecordKind::Dirents, payload))
                    });
            }
        }
    }

    fn apply_invalidations(&self, effects: &wit_types::Effects) -> (Vec<Path>, Vec<Path>) {
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
                    let Ok(path) = Path::parse(p) else {
                        warn!(
                            path = p.as_str(),
                            "invalidation effect used an invalid protocol path; rejecting"
                        );
                        continue;
                    };
                    self.store.delete_listing_path(&path);
                    invalidated_paths.push(path);
                },
                wit_types::Invalidation::Listing(wit_types::PathOrPrefix::Prefix(p)) => {
                    let Ok(prefix) = Path::parse(p) else {
                        warn!(
                            path = p.as_str(),
                            "invalidation effect used an invalid protocol prefix; rejecting"
                        );
                        continue;
                    };
                    self.store.delete_listing_prefix(&prefix);
                    invalidated_prefixes.push(prefix);
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
        cache_projection_batch(
            self.store,
            parent_path,
            std::iter::once(&entry.target).chain(entry.siblings.iter()),
            entry.exhaustive,
            ProjectionDirentsWrite::LookupHints,
        );
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
            DYNAMIC_TTL_MILLIS,
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
        cache_projection_batch(
            self.store,
            path,
            &listing.entries,
            listing.exhaustive,
            ProjectionDirentsWrite::AuthoritativeListing {
                validator: listing.validator.clone(),
                next_cursor: listing.next_cursor.clone().map(cached_cursor_from_wit),
            },
        );
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
        cache_projection_batch(
            self.store,
            path,
            entries,
            false,
            ProjectionDirentsWrite::Suppressed,
        );
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
        if self.rejects_conflicting_id(&[&entry_path], &oid) {
            return;
        }
        self.store
            .put_index_only(oid.as_bytes(), std::slice::from_ref(&entry_path), op_gen);
    }

    /// True if any of `paths` is already indexed to an id != `id` (a provider bug).
    fn rejects_conflicting_id(&self, paths: &[&Path], id: &ObjectId) -> bool {
        paths.iter().any(|p| {
            self.store.id_of_path(p).is_some_and(|existing| {
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

#[derive(Default)]
struct FsEffectRecords {
    batch: Vec<BatchRecord>,
    dirs: BTreeMap<Path, bool>,
    children: BTreeMap<Path, BTreeMap<String, DirentRecord>>,
}

fn split_projected_path(path: &Path) -> Option<(Path, String)> {
    let (parent, name) = path.parent_and_name()?;
    Some((parent, name.to_string()))
}

fn freshness_expiry(stability: Stability, now_millis: u64) -> Option<u64> {
    match stability {
        Stability::Stable => None,
        Stability::Dynamic => Some(now_millis.saturating_add(DYNAMIC_TTL_MILLIS)),
        Stability::Live => Some(now_millis),
    }
}

/// Push lookup + attr records for a projected path and its translated metadata.
fn push_projected_entry(batch: &mut Vec<BatchRecord>, path: &Path, meta: EntryMeta) {
    let lookup = LookupPayload::Positive(meta.clone());
    if let Some(payload) = lookup.serialize() {
        batch.push(BatchRecord::new(
            path.clone(),
            RecordKind::Lookup,
            None,
            Record::new(RecordKind::Lookup, payload),
        ));
    }
    let attr = AttrPayload { meta };
    if let Some(payload) = attr.serialize() {
        batch.push(BatchRecord::new(
            path.clone(),
            RecordKind::Attr,
            None,
            Record::new(RecordKind::Attr, payload),
        ));
    }
}

/// Push inline file content for a projected file when durable caching applies.
fn push_projected_file_content(
    batch: &mut Vec<BatchRecord>,
    file_path: &Path,
    file: &wit_types::FileOut,
) {
    let attrs_cache = file_attrs_from_file_out(file);
    if let Some(content) = attrs_cache.inline_bytes()
        && let Some(aux) = attrs_cache.durable_cache_aux()
    {
        let payload = FilePayload::new(attrs_cache.version_token_owned(), content.to_vec())
            .with_content_type(file.content_type.clone());
        if let Some(payload) = payload.serialize() {
            batch.push(BatchRecord::new(
                file_path.clone(),
                RecordKind::File,
                aux,
                Record::new(RecordKind::File, payload),
            ));
        }
    }
}

enum ProjectionDirentsWrite {
    Suppressed,
    AuthoritativeListing {
        validator: Option<String>,
        next_cursor: Option<CachedCursor>,
    },
    LookupHints,
}

impl ProjectionDirentsWrite {
    fn payload(
        self,
        store: &Store,
        parent_path: &Path,
        exhaustive: bool,
        dirent_map: BTreeMap<String, DirentRecord>,
    ) -> Option<DirentsPayload> {
        match self {
            Self::Suppressed => None,
            Self::AuthoritativeListing {
                validator,
                next_cursor,
            } => Some(DirentsPayload {
                entries: dirent_map.into_values().collect(),
                exhaustive,
                validator,
                paginated: next_cursor.is_some(),
                next_cursor,
            }),
            Self::LookupHints if exhaustive => Some(DirentsPayload {
                entries: dirent_map.into_values().collect(),
                exhaustive: true,
                validator: None,
                next_cursor: None,
                paginated: false,
            }),
            Self::LookupHints => {
                let existing_record = store
                    .cache_get(parent_path, RecordKind::Dirents, None)
                    .and_then(|record| DirentsPayload::deserialize(&record.payload));
                Some(DirentsPayload::merged(existing_record, dirent_map, false))
            },
        }
    }
}

fn cache_projection_batch<'a, I>(
    store: &Store,
    parent_path: &Path,
    entries: I,
    exhaustive: bool,
    dirents_write: ProjectionDirentsWrite,
) where
    I: IntoIterator<Item = &'a wit_types::DirEntry>,
{
    let entries: Vec<(&wit_types::DirEntry, EntryMeta)> = entries
        .into_iter()
        .filter_map(|entry| {
            if synthetic::is_reserved_provider_leaf(&entry.name) {
                warn!(
                    name = entry.name.as_str(),
                    parent = parent_path.as_str(),
                    "provider listing yielded a reserved '@'-prefixed entry; skipping"
                );
                return None;
            }
            Some((entry, entry_meta_from_kind(&entry.kind)))
        })
        .collect();

    let mut batch = Vec::new();
    let dirent_map = entries
        .iter()
        .map(|(entry, meta)| {
            (
                entry.name.clone(),
                DirentRecord {
                    name: entry.name.clone(),
                    meta: meta.clone(),
                },
            )
        })
        .collect();
    if let Some(dirents_payload) = dirents_write.payload(store, parent_path, exhaustive, dirent_map)
        && let Some(payload) = dirents_payload.serialize()
    {
        batch.push(BatchRecord::new(
            parent_path.clone(),
            RecordKind::Dirents,
            None,
            Record::new(RecordKind::Dirents, payload),
        ));
    }

    for (entry, meta) in entries {
        let path = parent_path
            .join(&entry.name)
            .expect("protocol path segment");
        push_projected_entry(&mut batch, &path, meta);
        if let wit_types::EntryKind::File(file) = &entry.kind {
            push_projected_file_content(&mut batch, &path, file);
        }
    }

    if !batch.is_empty() {
        debug!(
            target: "omnifs_engine_cache",
            kind = "projection",
            count = batch.len(),
            "caching direct projection result"
        );
        store.cache_put_batch(&batch);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::Caches;
    use std::sync::Arc;

    fn open_store(mount: &str) -> (tempfile::TempDir, Arc<Caches>, Store) {
        let dir = tempfile::tempdir().unwrap();
        let caches = Caches::open(dir.path()).unwrap();
        let store = caches.mount(mount);
        (dir, caches, store)
    }

    fn file_out(bytes: &[u8]) -> wit_types::FileOut {
        wit_types::FileOut {
            attrs: wit_types::FileAttrs {
                size: wit_types::FileSize::Exact(bytes.len() as u64),
                stability: wit_types::Stability::Stable,
                version_token: None,
            },
            bytes: wit_types::ByteSource::Inline(bytes.to_vec()),
            content_type: Some("/text/plain".to_string()),
        }
    }

    fn dir_entry(name: &str) -> wit_types::DirEntry {
        wit_types::DirEntry {
            id: None,
            name: name.to_string(),
            kind: wit_types::EntryKind::File(file_out(b"x")),
        }
    }

    fn p(raw: &str) -> Path {
        Path::parse(raw).unwrap()
    }

    fn put_paged_dirents(store: &Store, path: &str) {
        let payload = DirentsPayload {
            entries: vec![DirentRecord {
                name: "first".to_string(),
                meta: entry_meta_from_kind(&wit_types::EntryKind::File(file_out(b"first"))),
            }],
            exhaustive: false,
            validator: Some("etag-1".to_string()),
            next_cursor: Some(CachedCursor::Page(1)),
            paginated: true,
        }
        .serialize()
        .unwrap();
        let path = p(path);
        store.cache_put(
            &path,
            RecordKind::Dirents,
            None,
            &Record::new(RecordKind::Dirents, payload),
        );
    }

    fn cached_dirents(store: &Store, path: &str) -> DirentsPayload {
        let path = p(path);
        let record = store
            .cache_get(&path, RecordKind::Dirents, None)
            .expect("dirents record");
        DirentsPayload::deserialize(&record.payload).expect("dirents payload")
    }

    #[test]
    fn lookup_hints_merge_without_claiming_listing_authority() {
        let (_dir, _caches, store) = open_store("test");
        put_paged_dirents(&store, "/hello/feed");
        let lookup = wit_types::LookupEntry {
            target: dir_entry("preloaded"),
            siblings: Vec::new(),
            exhaustive: false,
        };

        EffectApplier::new(&store).apply_lookup_projection(&p("/hello/feed"), &lookup, 1);

        let dirents = cached_dirents(&store, "/hello/feed");
        assert!(
            dirents
                .entries
                .iter()
                .any(|entry| entry.name == "preloaded")
        );
        assert_eq!(dirents.validator.as_deref(), Some("etag-1"));
        assert_eq!(dirents.next_cursor, Some(CachedCursor::Page(1)));
        assert!(dirents.paginated);
    }

    #[test]
    fn authoritative_listing_replaces_prior_dirents() {
        let (_dir, _caches, store) = open_store("test");
        put_paged_dirents(&store, "/hello/feed");
        let listing = wit_types::DirListing {
            entries: vec![dir_entry("only")],
            exhaustive: true,
            validator: None,
            next_cursor: None,
        };

        EffectApplier::new(&store).apply_listing_projection(&p("/hello/feed"), &listing, 1);

        let dirents = cached_dirents(&store, "/hello/feed");
        assert_eq!(dirents.entries.len(), 1);
        assert_eq!(dirents.entries[0].name, "only");
        assert!(dirents.exhaustive);
        assert!(!dirents.paginated);
        assert!(dirents.next_cursor.is_none());
    }

    #[test]
    fn continuation_projection_does_not_overwrite_dirents() {
        let (_dir, _caches, store) = open_store("test");
        put_paged_dirents(&store, "/hello/feed");

        EffectApplier::new(&store).apply_continuation_projection(
            &p("/hello/feed"),
            &[dir_entry("page-two")],
            1,
        );

        let dirents = cached_dirents(&store, "/hello/feed");
        assert_eq!(dirents.entries.len(), 1);
        assert_eq!(dirents.entries[0].name, "first");
        assert!(
            store
                .cache_get(&p("/hello/feed/page-two"), RecordKind::Lookup, None)
                .is_some()
        );
    }
}
