use crate::EngineError;
use crate::cache::RecordKind;
use crate::clock::now_millis;
use crate::coalesce::ns::{Key as NsKey, OrderKey, SharedError};
use crate::effect_apply::{EffectApplier, LookupOutcome};
use crate::object_id::ObjectId;
use crate::runtime::Namespace;
use crate::runtime::Result;
use crate::view::{AttrPayload, CachedCursor, EntryMeta, FileAttrsCache, Stability};
use omnifs_core::path::{Path, Segment};
use omnifs_wit::provider::types as wit_types;

#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: String,
    pub meta: EntryMeta,
}

#[derive(Debug, Clone)]
pub struct DirListing {
    pub entries: Vec<DirEntry>,
    pub exhaustive: bool,
    pub validator: Option<String>,
    pub next_cursor: Option<CachedCursor>,
}

#[derive(Debug, Clone)]
pub enum ListOutcome {
    Entries(DirListing),
    Unchanged,
    Subtree(u64),
}

#[derive(Debug, Clone)]
pub enum ReadBytes {
    Inline(Vec<u8>),
    Blob(u64),
    Canonical,
}

#[derive(Debug, Clone)]
pub struct ReadOutcome {
    pub attrs: FileAttrsCache,
    pub bytes: ReadBytes,
    pub content_type: Option<String>,
}

#[derive(Debug, Clone)]
pub struct OpenOutcome {
    pub handle: u64,
    pub attrs: FileAttrsCache,
}

#[derive(Debug, Clone)]
pub struct ChunkOutcome {
    pub content: Vec<u8>,
    pub eof: bool,
}

impl Namespace<'_> {
    pub async fn lookup_child(&self, parent_path: &Path, name: &str) -> Result<LookupOutcome> {
        let name = Segment::try_from(name)
            .map_err(|error| EngineError::ProviderProtocol(error.to_string()))?;
        let child_path = parent_path.join_segment(&name);
        let now = now_millis();
        if self.runtime.cache.negative_for(&child_path, now).is_some() {
            return Ok(LookupOutcome::NotFound);
        }
        let op_gen = self.runtime.cache().current_generation();
        let key = NsKey::Lookup(child_path.clone());
        let order_key = OrderKey::Path(child_path.clone());
        let result = self
            .runtime
            .coalesce
            .lookup(key.clone(), || async {
                self.runtime
                    .coalesce
                    .ordered(&order_key, || async {
                        self.runtime
                            .run_lookup_child(parent_path, &name)
                            .await
                            .map_err(SharedError::from)
                    })
                    .await
            })
            .await
            .map_err(SharedError::into_engine)?;
        Ok(EffectApplier::new(&self.runtime.cache).lookup(
            parent_path,
            &child_path,
            result,
            op_gen,
            now_millis(),
        ))
    }

    pub async fn list_children(
        &self,
        path: &Path,
        cached_validator: Option<String>,
        cursor: Option<CachedCursor>,
    ) -> Result<ListOutcome> {
        let is_continuation = cursor.is_some();
        let op_gen = self.runtime.cache().current_generation();
        let key = NsKey::List(path.clone());
        let order_key = OrderKey::Path(path.clone());
        let result = if is_continuation {
            self.runtime
                .coalesce
                .ordered(&order_key, || async {
                    self.runtime
                        .run_list_children(
                            path,
                            cached_validator,
                            cursor.map(crate::wit_protocol::cached_cursor_to_wit),
                        )
                        .await
                })
                .await?
        } else {
            self.runtime
                .coalesce
                .list(key.clone(), || async {
                    self.runtime
                        .coalesce
                        .ordered(&order_key, || async {
                            self.runtime
                                .run_list_children(
                                    path,
                                    cached_validator,
                                    cursor.map(crate::wit_protocol::cached_cursor_to_wit),
                                )
                                .await
                                .map_err(SharedError::from)
                        })
                        .await
                })
                .await
                .map_err(SharedError::into_engine)?
        };

        if let wit_types::ListChildrenResult::Entries(ref listing) = result {
            let m = EffectApplier::new(&self.runtime.cache);
            if is_continuation {
                m.apply_continuation_projection(path, &listing.entries, op_gen);
            } else {
                m.apply_listing_projection(path, listing, op_gen);
            }
        }

        Ok(ListOutcome::from_wit(result))
    }

    pub async fn read_file(&self, path: &Path, content_type: String) -> Result<ReadOutcome> {
        self.read_file_with_mode(path, content_type, ReadMode::Serve)
            .await
    }

    pub(crate) async fn revalidate_file(
        &self,
        path: &Path,
        content_type: String,
    ) -> Result<ReadOutcome> {
        self.read_file_with_mode(path, content_type, ReadMode::Revalidate)
            .await
    }

    async fn read_file_with_mode(
        &self,
        path: &Path,
        content_type: String,
        mode: ReadMode,
    ) -> Result<ReadOutcome> {
        let now = now_millis();
        if self.runtime.cache.negative_for(path, now).is_some() {
            return Err(enoent(path.as_str()));
        }

        // Single cache lookup: derive both the warm_id (for coalescing key and
        // live check) and the CanonicalInput (byte buffer for the provider).
        let (warm_id, cached_canonical) = match self.runtime.cache.cached_canonical_for(path) {
            Some(crate::cache::CachedCanonical {
                id,
                bytes,
                validator,
            }) => {
                let host_id = ObjectId::from_bytes(id);
                let canonical = host_id.to_wit().map(|id| wit_types::CanonicalInput {
                    id,
                    validator,
                    bytes,
                    revalidate: mode.revalidates(),
                });
                (Some(host_id), canonical)
            },
            None => (None, None),
        };

        let live = warm_id
            .as_ref()
            .and_then(|_| leaf_stability(self, path))
            .is_some_and(|s| s == Stability::Live);

        // Cheap op for the error arm: no byte buffer, same path/content_type shape.
        // Warm-but-not-live reads coalesce by object identity, so concurrent
        // user reads of distinct paths that alias the same object share one
        // provider operation. Timer revalidation uses a distinct object key
        // because a normal warm read may serve pushed bytes without reloading.
        // Cold reads have no known id yet, so they key on the path.
        let coalesce_key = match &warm_id {
            Some(host_id) => match mode {
                ReadMode::Serve => NsKey::ReadObject(host_id.clone()),
                ReadMode::Revalidate => NsKey::Revalidate(host_id.clone()),
            },
            None => NsKey::ReadPath(path.clone()),
        };
        let order_key = match &warm_id {
            Some(host_id) => match mode {
                ReadMode::Serve => OrderKey::Object(host_id.clone()),
                ReadMode::Revalidate => OrderKey::Revalidate(host_id.clone()),
            },
            None => OrderKey::Path(path.clone()),
        };
        let result = if live {
            self.runtime
                .coalesce
                .ordered(&order_key, || async {
                    self.runtime
                        .run_read_file(path, content_type, cached_canonical)
                        .await
                })
                .await?
        } else {
            self.runtime
                .coalesce
                .read(coalesce_key, || async {
                    self.runtime
                        .coalesce
                        .ordered(&order_key, || async {
                            self.runtime
                                .run_read_file(path, content_type, cached_canonical)
                                .await
                                .map_err(SharedError::from)
                        })
                        .await
                })
                .await
                .map_err(SharedError::into_engine)?
        };

        match result {
            wit_types::ReadFileOutcome::Found(result) => {
                match warm_id {
                    Some(host_id) => self.runtime.note_read_object(host_id),
                    None => {
                        if let Some(canonical) = self.runtime.cache.cached_canonical_for(path) {
                            self.runtime
                                .note_read_object(ObjectId::from_bytes(canonical.id));
                        }
                    },
                }
                Ok(ReadOutcome::from_wit(result))
            },
            wit_types::ReadFileOutcome::NotFound(_) => Err(enoent(path.as_str())),
        }
    }

    pub async fn open_file(&self, path: &Path) -> Result<OpenOutcome> {
        self.runtime
            .run_open_file(path)
            .await
            .map(OpenOutcome::from_wit)
    }

    pub async fn read_chunk(&self, handle: u64, offset: u64, length: u32) -> Result<ChunkOutcome> {
        self.runtime
            .run_read_chunk(handle, offset, length)
            .await
            .map(ChunkOutcome::from_wit)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReadMode {
    Serve,
    Revalidate,
}

impl ReadMode {
    fn revalidates(self) -> bool {
        self == Self::Revalidate
    }
}

impl ListOutcome {
    fn from_wit(result: wit_types::ListChildrenResult) -> Self {
        match result {
            wit_types::ListChildrenResult::Entries(listing) => Self::Entries(DirListing {
                entries: listing
                    .entries
                    .into_iter()
                    .map(DirEntry::from_wit)
                    .collect(),
                exhaustive: listing.exhaustive,
                validator: listing.validator,
                next_cursor: listing
                    .next_cursor
                    .map(crate::wit_protocol::cached_cursor_from_wit),
            }),
            wit_types::ListChildrenResult::Unchanged => Self::Unchanged,
            wit_types::ListChildrenResult::Subtree(tree_ref) => Self::Subtree(tree_ref),
        }
    }
}

impl DirEntry {
    fn from_wit(entry: wit_types::DirEntry) -> Self {
        Self {
            name: entry.name,
            meta: crate::wit_protocol::entry_meta_from_kind(&entry.kind),
        }
    }
}

impl ReadOutcome {
    fn from_wit(result: wit_types::ReadFileResult) -> Self {
        Self {
            attrs: crate::wit_protocol::file_attrs_from_attrs(&result.attrs),
            bytes: ReadBytes::from_wit(result.bytes),
            content_type: result.content_type,
        }
    }
}

impl ReadBytes {
    fn from_wit(bytes: wit_types::ByteSource) -> Self {
        match bytes {
            wit_types::ByteSource::Inline(bytes) => Self::Inline(bytes),
            wit_types::ByteSource::Blob(blob) => Self::Blob(blob),
            wit_types::ByteSource::Canonical => Self::Canonical,
            // The validator rejects a `deferred` read answer before this path is
            // reached; keep a conservative empty inline value if the invariant
            // is ever violated after validation.
            wit_types::ByteSource::Deferred(_) => Self::Inline(Vec::new()),
        }
    }
}

impl OpenOutcome {
    fn from_wit(result: wit_types::OpenFileResult) -> Self {
        Self {
            handle: result.handle,
            attrs: FileAttrsCache::deferred(
                crate::wit_protocol::file_size_from_wit(result.attrs.size),
                crate::view::ReadMode::Ranged,
                crate::wit_protocol::stability_from_wit(result.attrs.stability),
                result.attrs.version_token,
            )
            .expect("provider open attrs are validated before view conversion"),
        }
    }
}

impl ChunkOutcome {
    fn from_wit(result: wit_types::ReadChunkResult) -> Self {
        Self {
            content: result.content,
            eof: result.eof,
        }
    }
}

fn leaf_stability(ns: &Namespace<'_>, path: &Path) -> Option<Stability> {
    ns.runtime
        .cache()
        .cache_get(path, RecordKind::Attr, None)
        .and_then(|record| AttrPayload::deserialize(&record.payload))
        .and_then(|attr| attr.meta.attrs().map(FileAttrsCache::stability))
}

fn enoent(path: &str) -> EngineError {
    EngineError::ProviderError(wit_types::ProviderError {
        kind: wit_types::ErrorKind::NotFound,
        message: format!("no such file: {path}"),
        retryable: false,
        retry_after: None,
    })
}
