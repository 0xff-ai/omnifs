use crate::cache::{
    AttrPayload, BatchRecord, CacheRecord, DirentRecord, DirentsPayload, EntryMeta, FilePayload,
    LookupPayload, RecordKind,
};
use crate::omnifs::provider::types as wit_types;
use crate::runtime::ProviderRuntime;

#[derive(Default)]
pub(super) struct ProjectionAccumulator {
    /// Per-dir flag indicating whether the children projected in this
    /// batch constitute the authoritative listing of that dir. `true`
    /// here causes `merge_projected_dirs` to write the resulting dirents
    /// record with `exhaustive: true` (and to replace any pre-existing
    /// entries) rather than carrying forward stale listings.
    dirs: std::collections::BTreeMap<String, bool>,
    children: std::collections::BTreeMap<String, std::collections::BTreeMap<String, DirentRecord>>,
}

impl ProjectionAccumulator {
    pub(super) fn add(&mut self, entry: &wit_types::ProjEntry, batch: &mut Vec<BatchRecord>) {
        if matches!(entry.kind, wit_types::EntryKind::Directory) {
            // `listing_exhaustive` is sticky once true within a batch: a
            // later non-exhaustive projection of the same dir must not
            // demote an earlier exhaustive declaration.
            let existing = self.dirs.entry(entry.path.clone()).or_insert(false);
            *existing = *existing || entry.listing_exhaustive;
        }
        if let Some((parent, name)) = split_projected_path(&entry.path) {
            let name = name.to_string();
            self.children.entry(parent.to_string()).or_default().insert(
                name.clone(),
                DirentRecord {
                    name,
                    meta: EntryMeta::from(&entry.kind),
                },
            );
        }
        push_projected_entry(batch, &entry.path, &entry.kind);
        if let wit_types::EntryKind::File(file) = &entry.kind {
            push_projected_file_content(batch, &entry.path, file);
        }
    }
}

pub(crate) fn push_projected_file_content(
    batch: &mut Vec<BatchRecord>,
    file_path: &str,
    file: &wit_types::FileProj,
) {
    let attrs_cache = crate::cache::FileAttrsCache::from(file);
    if let Some(content) = attrs_cache.inline_bytes()
        && let Some(aux) = attrs_cache.durable_cache_aux()
    {
        let payload = FilePayload::new(attrs_cache.version_token.clone(), content.to_vec());
        if let Some(payload) = payload.serialize() {
            batch.push(BatchRecord::new(
                file_path,
                RecordKind::File,
                aux,
                CacheRecord::new(RecordKind::File, payload),
            ));
        }
    }
}

pub(crate) fn push_projected_entry(
    batch: &mut Vec<BatchRecord>,
    path: &str,
    kind: &wit_types::EntryKind,
) {
    let meta = EntryMeta::from(kind);
    let lookup = LookupPayload::Positive(meta.clone());
    if let Some(payload) = lookup.serialize() {
        batch.push(BatchRecord::new(
            path,
            RecordKind::Lookup,
            None,
            CacheRecord::new(RecordKind::Lookup, payload),
        ));
    }

    let attr = AttrPayload { meta };
    if let Some(payload) = attr.serialize() {
        batch.push(BatchRecord::new(
            path,
            RecordKind::Attr,
            None,
            CacheRecord::new(RecordKind::Attr, payload),
        ));
    }
}

pub(super) fn split_projected_path(path: &str) -> Option<(&str, &str)> {
    let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
    (!name.is_empty()).then_some((parent, name))
}

impl ProviderRuntime {
    pub(super) fn apply_effects(&self, effects: &[wit_types::Effect]) {
        let mut batch = Vec::new();
        let mut projections = ProjectionAccumulator::default();
        for effect in effects {
            match effect {
                wit_types::Effect::Project(entry) => projections.add(entry, &mut batch),
                wit_types::Effect::InvalidatePath(path) => {
                    self.cache_delete_path(path);
                    self.invalidation.record_path(path.clone());
                },
                wit_types::Effect::InvalidatePrefix(prefix) => {
                    self.cache_delete_prefix(prefix);
                    self.invalidation.record_prefix(prefix.clone());
                },
                wit_types::Effect::DisownTree(_) => {},
            }
        }
        self.merge_projected_dirs(projections, &mut batch);
        if !batch.is_empty() {
            tracing::debug!(target: "omnifs_cache", kind = "project", count = batch.len(), "applying projection effects");
            self.cache_put_batch(&batch);
        }
    }

    fn merge_projected_dirs(
        &self,
        projections: ProjectionAccumulator,
        batch: &mut Vec<BatchRecord>,
    ) {
        let ProjectionAccumulator { dirs, mut children } = projections;
        for (dir, listing_exhaustive) in dirs {
            let Some(new_children) = children.remove(&dir) else {
                continue;
            };
            let (entries, exhaustive) = if listing_exhaustive {
                // The provider asserted this batch fully describes the
                // dir's children. Drop any stale pre-existing entries
                // and mark the record exhaustive.
                (new_children, true)
            } else {
                let (previously_exhaustive, mut existing) = self
                    .cache_get(&dir, RecordKind::Dirents, None)
                    .and_then(|record| DirentsPayload::deserialize(&record.payload))
                    .map_or_else(
                        || (false, std::collections::BTreeMap::new()),
                        |payload| {
                            (
                                payload.exhaustive,
                                payload
                                    .entries
                                    .into_iter()
                                    .map(|e| (e.name.clone(), e))
                                    .collect(),
                            )
                        },
                    );
                let introduced = new_children.keys().any(|n| !existing.contains_key(n));
                existing.extend(new_children);
                (existing, previously_exhaustive && !introduced)
            };
            if let Some(payload) = (DirentsPayload {
                entries: entries.into_values().collect(),
                exhaustive,
            })
            .serialize()
            {
                batch.push(BatchRecord::new(
                    dir,
                    RecordKind::Dirents,
                    None,
                    CacheRecord::new(RecordKind::Dirents, payload),
                ));
            }
        }
    }
}
