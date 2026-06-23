//! Host-owned browse projection into the view cache.
//!
//! Translates provider browse results into `BatchRecord` batches and calls
//! cache storage primitives. The cache crate stores bytes; this module decides
//! what provider listings and lookups mean for the view layer.

use std::collections::BTreeMap;

use omnifs_cache::{BatchRecord, Record, RecordKind, Store};
use omnifs_core::path::Path;
use omnifs_core::view::{
    AttrPayload, CachedCursor, DirentRecord, DirentsPayload, FilePayload, LookupPayload,
};
use tracing::{debug, warn};

use crate::pagination;
use crate::wit_protocol::{cached_cursor_from_wit, entry_meta_from_kind, file_attrs_from_file_out};
use omnifs_wit::provider::types as wit_types;

/// Cache the result of a `lookup-child` call, including sibling hints.
pub(crate) fn apply_lookup_projection(
    store: &Store,
    parent_path: &Path,
    entry: &wit_types::LookupEntry,
) {
    cache_projection_batch(
        store,
        parent_path,
        std::iter::once(&entry.target).chain(entry.siblings.iter()),
        entry.exhaustive,
        ProjectionDirentsWrite::LookupHints,
    );
}

/// Cache the authoritative listing from a `list-children` response.
pub(crate) fn apply_listing_projection(
    store: &Store,
    path: &Path,
    listing: &wit_types::DirListing,
) {
    cache_projection_batch(
        store,
        path,
        &listing.entries,
        listing.exhaustive,
        ProjectionDirentsWrite::AuthoritativeListing {
            validator: listing.validator.clone(),
            next_cursor: listing.next_cursor.clone().map(cached_cursor_from_wit),
        },
    );
}

/// Cache a continuation page from a paged `list-children` response.
pub(crate) fn apply_continuation_projection(
    store: &Store,
    path: &Path,
    entries: &[wit_types::DirEntry],
) {
    cache_projection_batch(
        store,
        path,
        entries,
        false,
        ProjectionDirentsWrite::Suppressed,
    );
}

/// Push lookup + attr records for a projected path/kind pair.
pub(crate) fn push_projected_entry(
    batch: &mut Vec<BatchRecord>,
    path: &Path,
    kind: &wit_types::EntryKind,
) {
    let meta = entry_meta_from_kind(kind);
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
pub(crate) fn push_projected_file_content(
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
    let entries: Vec<&wit_types::DirEntry> = entries
        .into_iter()
        .filter(|entry| {
            if pagination::is_reserved_provider_leaf(&entry.name) {
                warn!(
                    name = entry.name.as_str(),
                    parent = parent_path.as_str(),
                    "provider listing yielded a reserved '@'-prefixed entry; skipping"
                );
                return false;
            }
            true
        })
        .collect();

    let mut batch = Vec::new();
    let dirent_map = entries
        .iter()
        .map(|entry| {
            let meta = entry_meta_from_kind(&entry.kind);
            (
                entry.name.clone(),
                DirentRecord {
                    name: entry.name.clone(),
                    meta,
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

    for entry in &entries {
        let path = parent_path
            .join(&entry.name)
            .expect("protocol path segment");
        push_projected_entry(&mut batch, &path, &entry.kind);
        if let wit_types::EntryKind::File(file) = &entry.kind {
            push_projected_file_content(&mut batch, &path, file);
        }
    }

    if !batch.is_empty() {
        debug!(
            target: "omnifs_cache",
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
    use omnifs_cache::Caches;
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

        apply_lookup_projection(&store, &p("/hello/feed"), &lookup);

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

        apply_listing_projection(&store, &p("/hello/feed"), &listing);

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

        apply_continuation_projection(&store, &p("/hello/feed"), &[dir_entry("page-two")]);

        let dirents = cached_dirents(&store, "/hello/feed");
        assert_eq!(dirents.entries.len(), 1);
        assert_eq!(dirents.entries[0].name, "first");
        assert!(
            store
                .cache_get(&p("/hello/feed/page-two"), RecordKind::Lookup, None)
                .is_some()
        );
    }

    /// Regression: reading one child of an exhaustively-listed object directory
    /// must not collapse the cached dirents to that single child.
    ///
    /// An object dir (a GitHub issue dir, a k8s resource dir, an arxiv
    /// paper-version dir) is listed exhaustively, then a child lookup folds its
    /// `target + siblings` into the cached dirents. Because the lookup reports
    /// `exhaustive`, that fold REPLACES the listing with the lookup's entries.
    /// An honest object-dir lookup carries every other leaf as a sibling, so the
    /// replacement equals the full listing and `readdir` still enumerates all
    /// children. (The bug: a sibling-less exhaustive lookup shrank the dirents
    /// to the looked-up child, so `cat dir/body` then `ls dir` returned only
    /// `body`.)
    #[test]
    fn exhaustive_child_lookup_with_siblings_preserves_full_listing() {
        let (_dir, _caches, store) = open_store("test");
        let dir = "/o/r/issues/all/42";
        let children = ["body", "comments", "state", "title", "user"];

        // Cold exhaustive listing of the object dir.
        let listing = wit_types::DirListing {
            entries: children.iter().map(|n| dir_entry(n)).collect(),
            exhaustive: true,
            validator: None,
            next_cursor: None,
        };
        apply_listing_projection(&store, &p(dir), &listing);
        assert_eq!(cached_dirents(&store, dir).entries.len(), children.len());

        // `cat dir/body`: the lookup resolves `body` and, because an object's
        // leaf set is statically known and exhaustive, carries every OTHER leaf
        // as a sibling.
        let lookup = wit_types::LookupEntry {
            target: dir_entry("body"),
            siblings: children
                .iter()
                .filter(|n| **n != "body")
                .map(|n| dir_entry(n))
                .collect(),
            exhaustive: true,
        };
        apply_lookup_projection(&store, &p(dir), &lookup);

        let dirents = cached_dirents(&store, dir);
        let mut names: Vec<&str> = dirents.entries.iter().map(|e| e.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(
            names, children,
            "readdir must still enumerate every child after one child is read"
        );
    }

    /// Guards the inverse: a sibling-less exhaustive lookup is exactly the shape
    /// that collapses the directory, so the host fold is faithful and the fix
    /// must live at the lookup's source (the SDK carries the siblings). This
    /// documents WHY the host arm replaces wholesale and pins the failure mode.
    #[test]
    fn sibling_less_exhaustive_lookup_shrinks_listing() {
        let (_dir, _caches, store) = open_store("test");
        let dir = "/o/r/issues/all/42";
        let listing = wit_types::DirListing {
            entries: ["body", "title", "user"]
                .iter()
                .map(|n| dir_entry(n))
                .collect(),
            exhaustive: true,
            validator: None,
            next_cursor: None,
        };
        apply_listing_projection(&store, &p(dir), &listing);

        let lookup = wit_types::LookupEntry {
            target: dir_entry("body"),
            siblings: Vec::new(),
            exhaustive: true,
        };
        apply_lookup_projection(&store, &p(dir), &lookup);

        let dirents = cached_dirents(&store, dir);
        assert_eq!(
            dirents.entries.len(),
            1,
            "a sibling-less exhaustive lookup is treated as the whole directory; \
             the SDK must never emit one for a multi-leaf object dir"
        );
        assert_eq!(dirents.entries[0].name, "body");
    }
}
