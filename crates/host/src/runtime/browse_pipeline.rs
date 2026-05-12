use super::{CalloutRuntime, Op, Result, RuntimeError};
use crate::cache::{BatchRecord, CacheRecord, DirentRecord, DirentsPayload, EntryMeta, RecordKind};
use crate::omnifs::provider::types as wit_types;
use crate::runtime::inflight::{Acquired, share_outcome, unshare_outcome};
use std::collections::BTreeMap;
use tracing::debug;

impl CalloutRuntime {
    pub async fn lookup_child(
        &self,
        parent_path: &str,
        name: &str,
    ) -> Result<wit_types::LookupChildResult> {
        let child_path = child_path(parent_path, name);
        let result = self
            .coalesced(&child_path, || {
                self.call_provider_op_with_handoff_path(
                    Some(child_path.as_str()),
                    move |store, id| {
                        self.bindings.omnifs_provider_browse().call_lookup_child(
                            store,
                            id,
                            parent_path,
                            name,
                        )
                    },
                )
            })
            .await?;

        if let wit_types::OpResult::LookupChild(wit_types::LookupChildResult::Entry(entry)) =
            &result
        {
            self.touch_activity_for_relative_path(&child_path);
            self.cache_lookup_projection(parent_path, entry);
        }

        match result {
            wit_types::OpResult::LookupChild(result) => Ok(result),
            wit_types::OpResult::Error(error) => Err(RuntimeError::ProviderError(error)),
            result => Err(RuntimeError::UnexpectedOpResult {
                op: Op::LookupChild,
                result,
            }),
        }
    }

    pub async fn list_children(&self, path: &str) -> Result<wit_types::ListChildrenResult> {
        let result = self
            .coalesced(path, || {
                self.call_provider_op_with_handoff_path(Some(path), move |store, id| {
                    self.bindings
                        .omnifs_provider_browse()
                        .call_list_children(store, id, path)
                })
            })
            .await?;

        if let wit_types::OpResult::ListChildren(wit_types::ListChildrenResult::Entries(
            ref listing,
        )) = result
        {
            self.cache_projection_batch(path, &listing.entries, listing.exhaustive);
            self.touch_activity_for_relative_path(path);
        }

        match result {
            wit_types::OpResult::ListChildren(result) => Ok(result),
            wit_types::OpResult::Error(error) => Err(RuntimeError::ProviderError(error)),
            result => Err(RuntimeError::UnexpectedOpResult {
                op: Op::ListChildren,
                result,
            }),
        }
    }

    pub async fn read_file(&self, path: &str) -> Result<wit_types::ReadFileResult> {
        let result = self
            .coalesced(path, || {
                self.call_provider_op(move |store, id| {
                    self.bindings
                        .omnifs_provider_browse()
                        .call_read_file(store, id, path)
                })
            })
            .await?;

        if matches!(result, wit_types::OpResult::ReadFile(_)) {
            self.touch_activity_for_relative_path(path);
        }

        match result {
            wit_types::OpResult::ReadFile(result) => Ok(result),
            wit_types::OpResult::Error(error) => Err(RuntimeError::ProviderError(error)),
            result => Err(RuntimeError::UnexpectedOpResult {
                op: Op::ReadFile,
                result,
            }),
        }
    }

    pub async fn open_file(&self, path: &str) -> Result<wit_types::OpenFileResult> {
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

        match result {
            wit_types::OpResult::OpenFile(result) => Ok(result),
            wit_types::OpResult::Error(error) => Err(RuntimeError::ProviderError(error)),
            result => Err(RuntimeError::UnexpectedOpResult {
                op: Op::OpenFile,
                result,
            }),
        }
    }

    pub async fn read_chunk(
        &self,
        handle: u64,
        offset: u64,
        length: u32,
    ) -> Result<wit_types::ReadChunkResult> {
        let result = self
            .call_provider_op(move |store, id| {
                self.bindings
                    .omnifs_provider_browse()
                    .call_read_chunk(store, id, handle, offset, length)
            })
            .await?;

        match result {
            wit_types::OpResult::ReadChunk(result) => Ok(result),
            wit_types::OpResult::Error(error) => Err(RuntimeError::ProviderError(error)),
            result => Err(RuntimeError::UnexpectedOpResult {
                op: Op::ReadChunk,
                result,
            }),
        }
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
                        return unshare_outcome(outcome, RuntimeError::ProviderProtocol);
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
        ) -> std::result::Result<wit_types::ProviderStep, wasmtime::Error>,
    {
        self.call_provider_op_with_handoff_path(None, f).await
    }

    pub(super) async fn call_provider_op_with_handoff_path<F>(
        &self,
        expected_handoff_path: Option<&str>,
        f: F,
    ) -> Result<wit_types::OpResult>
    where
        F: FnOnce(
            &mut wasmtime::Store<super::HostState>,
            u64,
        ) -> std::result::Result<wit_types::ProviderStep, wasmtime::Error>,
    {
        let id = self.correlations.allocate();

        let step = {
            let mut store = self.store.lock();
            f(&mut store, id)?
        };

        self.drive_provider(id, step, expected_handoff_path).await
    }

    fn cache_lookup_projection(&self, parent_path: &str, entry: &wit_types::LookupEntry) {
        let mut entries = Vec::with_capacity(1 + entry.siblings.len());
        entries.push(entry.target.clone());
        entries.extend(entry.siblings.iter().cloned());
        self.cache_projection_batch(parent_path, &entries, entry.exhaustive);
    }

    fn cache_projection_batch(
        &self,
        parent_path: &str,
        entries: &[wit_types::DirEntry],
        exhaustive: bool,
    ) {
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
            Self::push_projected_entry(&mut batch, &child_path, &entry.kind);
            if let wit_types::EntryKind::File(file) = &entry.kind {
                Self::push_projected_file_content(&mut batch, &child_path, file);
            }
        }

        if !batch.is_empty() {
            debug!(
                target: "omnifs_cache",
                kind = "projection",
                count = batch.len(),
                "caching direct projection result"
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
}

fn child_path(parent_path: &str, name: &str) -> String {
    if parent_path.is_empty() {
        name.to_string()
    } else {
        format!("{parent_path}/{name}")
    }
}
