use super::{CalloutRuntime, Result, RuntimeError};
use crate::cache::{self, BatchRecord, CacheRecord, RecordKind};
use crate::omnifs::provider::types::{self as wit_types, DirListing};
use crate::runtime::inflight::{Acquired, share_outcome, unshare_outcome};
use std::collections::BTreeMap;

impl CalloutRuntime {
    pub async fn call_lookup_child(
        &self,
        parent_path: &str,
        name: &str,
    ) -> Result<wit_types::OpResult> {
        let child_path = if parent_path.is_empty() {
            name.to_string()
        } else {
            format!("{parent_path}/{name}")
        };

        let result = self
            .coalesced(&child_path, || {
                self.call_provider_op(move |store, id| {
                    self.bindings.omnifs_provider_browse().call_lookup_child(
                        store,
                        id,
                        parent_path,
                        name,
                    )
                })
            })
            .await?;

        if let wit_types::OpResult::Lookup(wit_types::LookupResult::Entry(entry)) = &result {
            self.touch_activity_for_relative_path(&child_path);
            let entries: Vec<wit_types::DirEntry> = entry
                .siblings
                .iter()
                .map(|e| wit_types::DirEntry {
                    name: e.name.clone(),
                    kind: e.kind.clone(),
                })
                .collect();

            self.cache_projection_batch(
                &child_path,
                &entries,
                &entry.sibling_files,
                entry.exhaustive,
            );
        }

        Ok(Self::strip_projected_files(result))
    }

    pub async fn call_list_children(&self, path: &str) -> Result<wit_types::OpResult> {
        let result = self
            .coalesced(path, || {
                self.call_provider_op(move |store, id| {
                    self.bindings
                        .omnifs_provider_browse()
                        .call_list_children(store, id, path)
                })
            })
            .await?;

        if let wit_types::OpResult::List(wit_types::ListResult::Entries(ref listing)) = result {
            self.cache_projection_batch(path, &listing.entries, &[], listing.exhaustive);
            self.touch_activity_for_relative_path(path);
        }

        Ok(Self::strip_projected_files(result))
    }

    pub async fn call_read_file(&self, path: &str) -> Result<wit_types::OpResult> {
        let result = self
            .coalesced(path, || {
                self.call_provider_op(move |store, id| {
                    self.bindings
                        .omnifs_provider_browse()
                        .call_read_file(store, id, path)
                })
            })
            .await?;

        if let wit_types::OpResult::Read(ref file_result) = result {
            let parent_path = path.rsplit_once('/').map_or("", |(p, _)| p);
            let sibling_files = match file_result {
                wit_types::FileContentResult::Inline(inline) => &inline.sibling_files,
                wit_types::FileContentResult::Blob(blob) => &blob.sibling_files,
            };
            self.cache_sibling_files(parent_path, sibling_files);
            self.touch_activity_for_relative_path(path);
        }

        Ok(result)
    }

    pub async fn call_open_file(&self, path: &str) -> Result<wit_types::OpResult> {
        let result = self
            .call_provider_op(move |store, id| {
                self.bindings
                    .omnifs_provider_browse()
                    .call_open_file(store, id, path)
            })
            .await?;

        if matches!(result, wit_types::OpResult::OpenFile(_)) {
            self.touch_activity_for_relative_path(path);
        }

        Ok(result)
    }

    pub async fn call_read_chunk(
        &self,
        handle: u64,
        offset: u64,
        length: u32,
    ) -> Result<wit_types::OpResult> {
        self.call_provider_op(move |store, id| {
            self.bindings
                .omnifs_provider_browse()
                .call_read_chunk(store, id, handle, offset, length)
        })
        .await
    }

    async fn coalesced<F, Fu>(&self, path: &str, op: F) -> Result<wit_types::OpResult>
    where
        F: Fn() -> Fu,
        Fu: std::future::Future<Output = Result<wit_types::OpResult>>,
    {
        loop {
            match self.inflight.acquire(path) {
                Acquired::Leader { guard } => {
                    let result = op().await;
                    guard.complete(share_outcome(&result));
                    return result;
                },
                Acquired::ExactMatch { mut rx } => {
                    if let Ok(outcome) = rx.recv().await {
                        return unshare_outcome(outcome, RuntimeError::ProviderError);
                    }
                },
                Acquired::AncestorWait { mut rx } => {
                    let _ = rx.recv().await;
                },
            }
        }
    }

    pub(super) async fn call_provider_op<F>(&self, f: F) -> Result<wit_types::OpResult>
    where
        F: FnOnce(
            &mut wasmtime::Store<super::HostState>,
            u64,
        ) -> std::result::Result<wit_types::ProviderReturn, wasmtime::Error>,
    {
        let id = self.correlations.allocate();

        let response = {
            let mut store = self.store.lock();
            f(&mut store, id)?
        };

        self.drive_callouts(id, response).await
    }

    fn cache_projection_batch(
        &self,
        parent_path: &str,
        entries: &[wit_types::DirEntry],
        sibling_files: &[wit_types::ProjectedFile],
        exhaustive: bool,
    ) {
        use cache::{AttrPayload, DirentRecord, DirentsPayload, EntryMeta, LookupPayload};

        let mut batch = Vec::new();

        let mut dirent_map = BTreeMap::new();
        for entry in entries {
            let meta = EntryMeta::from(&entry.kind);
            dirent_map.insert(
                entry.name.clone(),
                DirentRecord {
                    name: entry.name.clone(),
                    meta,
                },
            );
        }
        for pf in sibling_files {
            let meta = EntryMeta::file(cache::FileAttrsCache::from(&pf.attrs));
            dirent_map
                .entry(pf.name.clone())
                .or_insert_with(|| DirentRecord {
                    name: pf.name.clone(),
                    meta,
                });
        }
        let dirents_payload = DirentsPayload {
            entries: dirent_map.into_values().collect(),
            exhaustive,
        };
        if let Some(payload) = dirents_payload.serialize() {
            batch.push(BatchRecord::new(
                parent_path.to_string(),
                RecordKind::Dirents,
                None,
                CacheRecord::new(RecordKind::Dirents, payload),
            ));
        }

        for entry in entries {
            let child_path = if parent_path.is_empty() {
                entry.name.clone()
            } else {
                format!("{parent_path}/{}", entry.name)
            };

            let meta = EntryMeta::from(&entry.kind);

            let lookup = LookupPayload::Positive(meta.clone());
            if let Some(payload) = lookup.serialize() {
                batch.push(BatchRecord::new(
                    child_path.clone(),
                    RecordKind::Lookup,
                    None,
                    CacheRecord::new(RecordKind::Lookup, payload),
                ));
            }

            let attr = AttrPayload { meta };
            if let Some(payload) = attr.serialize() {
                batch.push(BatchRecord::new(
                    child_path.clone(),
                    RecordKind::Attr,
                    None,
                    CacheRecord::new(RecordKind::Attr, payload),
                ));
            }
        }

        for pf in sibling_files {
            let file_path = if parent_path.is_empty() {
                pf.name.clone()
            } else {
                format!("{parent_path}/{}", pf.name)
            };
            Self::push_projected_file(&mut batch, &file_path, &pf.attrs);
        }

        if !batch.is_empty() {
            tracing::debug!(
                target: "omnifs_cache",
                kind = "projection",
                count = batch.len(),
                "caching projection batch"
            );
            self.cache_put_batch(&batch);
        }
    }

    fn cache_sibling_files(&self, parent_path: &str, sibling_files: &[wit_types::ProjectedFile]) {
        let mut batch = Vec::new();
        for pf in sibling_files {
            let file_path = if parent_path.is_empty() {
                pf.name.clone()
            } else {
                format!("{parent_path}/{}", pf.name)
            };
            Self::push_projected_file(&mut batch, &file_path, &pf.attrs);
        }

        if !batch.is_empty() {
            tracing::debug!(
                target: "omnifs_cache",
                kind = "projection",
                count = batch.len(),
                "caching sibling files"
            );
            self.cache_put_batch(&batch);
        }
    }

    fn touch_activity_for_relative_path(&self, path: &str) {
        let absolute = super::absolute_mount_path(path);
        let mut best_by_depth = BTreeMap::new();
        for mount in &self.declared_handlers {
            let Some(concrete_path) = mount.concrete_path_for(&absolute) else {
                continue;
            };
            match best_by_depth.entry(mount.pattern_len()) {
                std::collections::btree_map::Entry::Vacant(slot) => {
                    slot.insert((mount, concrete_path));
                },
                std::collections::btree_map::Entry::Occupied(mut slot) => {
                    let current = slot.get().0;
                    if mount
                        .specificity()
                        .iter()
                        .cmp(current.specificity().iter())
                        .is_gt()
                    {
                        slot.insert((mount, concrete_path));
                    }
                },
            }
        }
        let touched = best_by_depth
            .into_values()
            .map(|(mount, concrete_path)| {
                (
                    mount.mount_id.clone(),
                    mount.mount_name.clone(),
                    concrete_path,
                )
            })
            .collect::<Vec<_>>();
        if touched.is_empty() {
            return;
        }
        self.activity_table.lock().touch(touched);
    }

    /// Strip listing-carried preload data before the result is stored or
    /// surfaced to the FUSE layer. These land at the response boundary via
    /// `apply_terminal_boundary`; they do not belong in the cached form.
    fn strip_projected_files(result: wit_types::OpResult) -> wit_types::OpResult {
        match result {
            wit_types::OpResult::List(wit_types::ListResult::Entries(listing)) => {
                wit_types::OpResult::List(wit_types::ListResult::Entries(DirListing {
                    entries: listing.entries,
                    exhaustive: listing.exhaustive,
                    preload: Vec::new(),
                }))
            },
            wit_types::OpResult::Lookup(wit_types::LookupResult::Entry(mut entry)) => {
                entry.preload = Vec::new();
                wit_types::OpResult::Lookup(wit_types::LookupResult::Entry(entry))
            },
            other => other,
        }
    }
}
