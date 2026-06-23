//! Whole-file read result types and the `Tree::read` body.
//!
//! This is the renderer-neutral home of the read-path DECISION logic that FUSE
//! otherwise carries in `read.rs` + `read_helpers.rs`: the cache cascade, the
//! per-mount `op_gen` write fence, the canonical-not-copied hybrid, and
//! learned-size promotion. It is fully async (no `block_on`): the renderer
//! drives `Tree::read` from its own executor and turns the neutral
//! `ReadResult` into kernel/protocol identity + reply encoding.

use std::path::PathBuf;

use omnifs_cache::{Record as CacheRecord, RecordKind};
use omnifs_core::view as view_types;
use omnifs_core::view::{AttrPayload, EntryMeta, FileAttrsCache, FilePayload, LookupPayload};
use omnifs_host::pagination::{self, NextPageOutcome};
use omnifs_host::wit_protocol::file_attrs_from_attrs;
use omnifs_host::{Error, Runtime};
use omnifs_wit::provider::types::{ByteSource, ReadFileResult};
use tracing::warn;

use crate::error::{Result, TreeError};
use crate::node::{Node, PaginationControl, SyntheticContent};
use crate::{RequestCtx, Tree};

/// Result of `Tree::read`. A two-arm shape so a treeref-backed node (read via
/// renderer std::fs passthrough over a real dir) can never be confused with
/// resolved provider bytes. `Bytes.attrs` is the POST-read learned attrs (exact
/// size promoted from the bytes) the renderer applies to st_size / the NFSv4
/// change attribute; `content_type` echoes the rendered representation type. On
/// a cache hit it is the node's projected attrs (the size was already learned
/// when the entry was first materialized).
#[derive(Debug, Clone)]
pub enum ReadResult {
    Bytes {
        data: Vec<u8>,
        attrs: Option<FileAttrsCache>,
        content_type: Option<String>,
    },
    Backing(PathBuf),
}

/// One ranged chunk from a `RangedHandle`. `learned_attrs` is `Some` on an
/// EOF-short read when an exact size was learned, so the renderer promotes
/// st_size (preserves today's `learned_ranged_eof_attrs` behavior).
#[derive(Debug, Clone)]
pub struct Chunk {
    pub bytes: Vec<u8>,
    pub eof: bool,
    pub learned_attrs: Option<FileAttrsCache>,
}

pub(crate) struct FileAttrStore<'a> {
    runtime: &'a Runtime,
    path: &'a omnifs_core::path::Path,
}

impl<'a> FileAttrStore<'a> {
    pub(crate) fn new(runtime: &'a Runtime, path: &'a omnifs_core::path::Path) -> Self {
        Self { runtime, path }
    }

    pub(crate) fn cached(&self) -> Option<FileAttrsCache> {
        if let Some(record) = self
            .runtime
            .cache()
            .cache_get(self.path, RecordKind::Lookup, None)
            && let Some(LookupPayload::Positive(meta)) = LookupPayload::deserialize(&record.payload)
            && let Some(attrs) = meta.into_attrs()
        {
            return Some(attrs);
        }

        self.runtime
            .cache()
            .cache_get(self.path, RecordKind::Attr, None)
            .and_then(|record| AttrPayload::deserialize(&record.payload))
            .and_then(|payload| payload.meta.into_attrs())
    }

    pub(crate) fn publish(&self, attrs: FileAttrsCache) -> Result<()> {
        let meta = EntryMeta::file(attrs);
        let lookup = LookupPayload::Positive(meta.clone());
        if let Some(payload) = lookup.serialize() {
            self.runtime.cache().cache_put(
                self.path,
                RecordKind::Lookup,
                None,
                &CacheRecord::new(RecordKind::Lookup, payload),
            );
        } else {
            return Err(TreeError::internal(format!(
                "could not serialize lookup attrs for {}",
                self.path
            )));
        }

        let attr = AttrPayload { meta };
        if let Some(payload) = attr.serialize() {
            self.runtime.cache().cache_put(
                self.path,
                RecordKind::Attr,
                None,
                &CacheRecord::new(RecordKind::Attr, payload),
            );
            Ok(())
        } else {
            Err(TreeError::internal(format!(
                "could not serialize file attrs for {}",
                self.path
            )))
        }
    }
}

impl Tree {
    pub fn cached_file_attrs(
        &self,
        mount: &str,
        path: &omnifs_core::path::Path,
    ) -> Option<FileAttrsCache> {
        let runtime = self.runtime_for(mount).ok()?;
        FileAttrStore::new(&runtime, path).cached()
    }

    pub fn publish_file_attrs(
        &self,
        mount: &str,
        path: &omnifs_core::path::Path,
        attrs: FileAttrsCache,
    ) -> Result<()> {
        let runtime = self.runtime_for(mount)?;
        FileAttrStore::new(&runtime, path).publish(attrs)
    }

    /// Whole-file read. Faithful port of the FUSE whole-file read DECISION
    /// logic: the read cache cascade (exact-0 short-circuit, mem hit, durable
    /// view hit, backing-fs read), then on a view miss the cold provider render
    /// fenced by the per-mount `op_gen`, with the canonical-not-copied hybrid
    /// and learned-size promotion.
    ///
    /// The renderer still owns its kernel-side handle caches (the per-`fh` whole
    /// buffer FUSE keeps, the inode size promotion) and kernel offset/size
    /// slicing; `Tree::read` returns the whole rendered file.
    pub async fn read(&self, node: &Node, ctx: &RequestCtx) -> Result<ReadResult> {
        // A host-synthesized node (a mount-root ignore file or a `@next`/`@all`
        // pagination control) is served by `Tree`, never the provider. The
        // renderer materializes the result into a per-handle buffer so a partial
        // or repeated read never re-runs the (mutating) control action.
        if let Some(synthetic) = node.synthetic_kind() {
            return self.read_synthetic(node, &synthetic.content, ctx).await;
        }

        // A treeref-backed node is served by the renderer from the real backing
        // dir; `Tree` hands the path back without a provider round trip.
        if let Some(dir) = node.subtree_path() {
            return Ok(ReadResult::Backing(dir.clone()));
        }

        let runtime = self.runtime_for(node.mount())?;
        let path = node.path();
        let attr_store = FileAttrStore::new(&runtime, path);
        let projected_attrs = attr_store.cached().or_else(|| node.attrs().cloned());
        let attrs = projected_attrs.as_ref();

        // Exact-0 short-circuit: a file the projection sizes at exactly zero is
        // empty without any provider call.
        if let Some(attrs) = attrs
            && matches!(attrs.size(), view_types::FileSize::Exact(0))
        {
            return Ok(ReadResult::Bytes {
                data: Vec::new(),
                attrs: Some(attrs.clone()),
                content_type: None,
            });
        }

        // Inline projected bytes are already the canonical answer for this view
        // leaf. Serve them here instead of forcing each frontend to decode cached
        // lookup/attr payloads or know whether a provider file route exists.
        if let Some(attrs) = attrs
            && let Some(bytes) = attrs.inline_bytes()
        {
            let data = bytes.to_vec();
            let attrs = attrs
                .learned_complete_content_attrs(data.len())
                .map_err(|error| {
                    TreeError::internal(format!(
                        "inline projection for {path} contradicts file attrs: {error}"
                    ))
                })?;
            attr_store.publish(attrs.clone())?;
            return Ok(ReadResult::Bytes {
                data,
                attrs: Some(attrs),
                content_type: None,
            });
        }

        let durable_aux = attrs.and_then(FileAttrsCache::durable_cache_aux);

        // Read cache cascade: mem (the FUSE pagination/in-memory tier), then the
        // durable view cache. Both are keyed by the durable aux and validated
        // against the projected attrs. A hit serves the cached bytes and keeps
        // the node's projected (already size-learned) attrs.
        if let Some(aux) = durable_aux.clone() {
            if let Some(record) = runtime
                .cache()
                .mem_get(path, RecordKind::File, aux.as_deref())
                && let Some(payload) = file_payload_for_attrs(&record, attrs)
            {
                return read_result_from_cache(path, payload, attrs);
            }
            if let Some(record) = runtime
                .cache()
                .cache_get(path, RecordKind::File, aux.as_deref())
                && let Some(payload) = file_payload_for_attrs(&record, attrs)
            {
                return read_result_from_cache(path, payload, attrs);
            }
        }

        // Cold miss. Derive the content type the host echoes into `read-file`:
        // the path's representation suffix wins, else octet-stream (the
        // SDK-supplied content type is unavailable on a cold read).
        let content_type = node.path().content_type_mime(None).to_string();

        // Capture the generation BEFORE awaiting the render so the result can be
        // fenced against an invalidation that lands mid-read.
        let op_gen = runtime.cache().current_generation();
        let result = match runtime
            .namespace()
            .read_file(path, content_type, ctx.trace)
            .await
        {
            Ok(result) => result,
            Err(Error::ProviderError(error)) => {
                warn!(
                    path = %path,
                    kind = ?error.kind,
                    retryable = error.retryable,
                    message = error.message,
                    "provider returned typed error for read_file"
                );
                return Err(Error::ProviderError(error).into());
            },
            Err(error) => {
                warn!(path = %path, error = %error, "read_file runtime error");
                return Err(error.into());
            },
        };

        finish_read(&runtime, path, result, op_gen)
    }

    /// Serve a host-synthesized node. A `Fixed` synthetic (a mount-root ignore
    /// file) returns its static bytes with the node's projected attrs; a
    /// `PaginationControl` runs the accumulating pagination action over the
    /// parent directory, invalidates the parent's cached dirents so a later
    /// listing reflects the grown feed, and returns a one-line status with a
    /// learned exact size so `cat` reads the whole message.
    async fn read_synthetic(
        &self,
        node: &Node,
        content: &SyntheticContent,
        ctx: &RequestCtx,
    ) -> Result<ReadResult> {
        match content {
            SyntheticContent::Fixed(bytes) => Ok(ReadResult::Bytes {
                data: bytes.clone(),
                attrs: node.attrs().cloned(),
                content_type: None,
            }),
            SyntheticContent::PaginationControl(action) => {
                let runtime = self.runtime_for(node.mount())?;
                let Some((parent, _)) = node.path().parent_and_name() else {
                    return Err(TreeError::invalid_input(format!(
                        "pagination control has no parent: {}",
                        node.path().as_str()
                    )));
                };
                let status = match action {
                    PaginationControl::All => runtime.paginate_all(&parent, ctx.trace).await,
                    PaginationControl::Next => {
                        match runtime.paginate_next(&parent, ctx.trace).await {
                            NextPageOutcome::Loaded { added, more } => format!(
                                "loaded +{added} entries; {}\n",
                                if more { "more available" } else { "complete" }
                            ),
                            NextPageOutcome::NoMore => "no more pages\n".to_string(),
                            NextPageOutcome::Error(message) => message,
                        }
                    },
                };
                // The action grew (or exhausted) the parent's accumulated
                // dirents; drop the parent's mem listing so a later browse
                // re-reads the stored record. The kernel re-list notify stays
                // renderer-side (driven from the InvalidationReport).
                runtime
                    .cache()
                    .mem_invalidate(&parent, RecordKind::Dirents, None);
                let bytes = status.into_bytes();
                let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                Ok(ReadResult::Bytes {
                    data: bytes,
                    attrs: Some(pagination::control_read_attrs(len)),
                    content_type: None,
                })
            },
        }
    }
}

/// Resolve a cold `read-file` terminal into bytes + learned attrs, validate, and
/// durably cache unless the canonical hybrid forbids it or the write fence
/// rejects it.
fn finish_read(
    runtime: &Runtime,
    path: &omnifs_core::path::Path,
    result: ReadFileResult,
    op_gen: u64,
) -> Result<ReadResult> {
    // An identity representation answered by reference to the canonical store
    // (`byte-source::canonical`) is NEVER copied into the view cache: the
    // canonical store is its sole home, so caching it here would duplicate the
    // bytes across both stores (ADR-0001 §4, hybrid policy).
    let from_canonical = matches!(result.bytes, ByteSource::Canonical);

    let Some((data, result_attrs, content_type)) = resolve_read_payload(runtime, path, result)
    else {
        return Err(TreeError::internal(format!(
            "read for {path} could not resolve its byte source"
        )));
    };

    let attrs_cache = result_attrs
        .learned_complete_content_attrs(data.len())
        .map_err(|error| {
            warn!(
                path = %path,
                expected = ?result_attrs.size(),
                actual = data.len(),
                error,
                "provider returned bytes that contradict file attrs"
            );
            TreeError::internal(format!(
                "read for {path} returned bytes that contradict file attrs"
            ))
        })?;
    FileAttrStore::new(runtime, path).publish(attrs_cache.clone())?;

    if !from_canonical {
        cache_durable_file_payload(
            runtime,
            path,
            &attrs_cache,
            &data,
            content_type.clone(),
            op_gen,
        )?;
    }

    Ok(ReadResult::Bytes {
        data,
        attrs: Some(attrs_cache),
        content_type,
    })
}

/// Cache the durable view payload for a freshly rendered cold read, honoring the
/// per-mount write fence. The captured `op_gen` predates the render, so a write
/// the fence rejects raced a concurrent invalidation and must be dropped
/// (caching it would reinstate stale bytes).
fn cache_durable_file_payload(
    runtime: &Runtime,
    path: &omnifs_core::path::Path,
    attrs_cache: &FileAttrsCache,
    data: &[u8],
    content_type: Option<String>,
    op_gen: u64,
) -> Result<()> {
    let Some(aux) = attrs_cache.durable_cache_aux() else {
        return Ok(());
    };
    let payload = FilePayload::new(attrs_cache.version_token_owned(), data.to_vec())
        .with_content_type(content_type);
    let Some(payload) = payload.serialize() else {
        return Err(TreeError::internal(format!(
            "read for {path} could not serialize its file payload"
        )));
    };
    let record = CacheRecord::new(RecordKind::File, payload);
    // Drop the write if an invalidation for this path landed after the read
    // began: caching it would reinstate stale bytes.
    if runtime.cache().write_fenced(path, op_gen) {
        return Ok(());
    }
    runtime
        .cache()
        .cache_put(path, RecordKind::File, aux.as_deref(), &record);
    Ok(())
}

/// Materialize a `read-file` terminal into bytes. Inline content travels in the
/// WIT; blob content is pulled from the host blob cache; `canonical` is served
/// from the anchor-keyed canonical store without copying across the WIT
/// (ADR-0001 §5.1). Returns `None` when the byte source can't be resolved
/// (logged at warn for diagnostics).
fn resolve_read_payload(
    runtime: &Runtime,
    path: &omnifs_core::path::Path,
    result: ReadFileResult,
) -> Option<(Vec<u8>, FileAttrsCache, Option<String>)> {
    let attrs = file_attrs_from_attrs(&result.attrs);
    let content_type = result.content_type;
    match result.bytes {
        ByteSource::Inline(bytes) => Some((bytes, attrs, content_type)),
        ByteSource::Blob(blob) => match runtime.read_blob_full(blob) {
            Ok(bytes) => Some((bytes, attrs, content_type)),
            Err(e) => {
                warn!(path = %path, error = %e, "blob-backed read failed");
                None
            },
        },
        ByteSource::Canonical => {
            if let Some(bytes) = runtime.canonical_bytes_for(path) {
                Some((bytes, attrs, content_type))
            } else {
                warn!(
                    path = %path,
                    "read answered byte-source::canonical but no canonical covers the path"
                );
                None
            }
        },
        // The validator rejects a `deferred` read answer before the read path is
        // reached; a read must produce bytes.
        ByteSource::Deferred(_) => {
            warn!(
                path = %path,
                "read answered byte-source::deferred, which is not a valid read answer"
            );
            None
        },
    }
}

/// Validate a cached `File` record against the projected attrs, returning the
/// decoded payload only when it is still a faithful answer for the projection.
fn file_payload_for_attrs(
    record: &CacheRecord,
    attrs: Option<&FileAttrsCache>,
) -> Option<FilePayload> {
    let payload = FilePayload::deserialize(&record.payload)?;
    let attrs = attrs?;
    if matches!(attrs.stability(), view_types::Stability::Dynamic)
        && payload.version_token.as_deref() != attrs.version_token()
    {
        return None;
    }
    if attrs
        .validate_complete_content(payload.content.len())
        .is_err()
    {
        return None;
    }
    Some(payload)
}

/// A cache hit serves complete cached bytes, so the read result can learn the
/// exact size the same way a cold provider read does. Most hits already carry a
/// learned size from an earlier read; preloaded file payloads can arrive before
/// any renderer has promoted the placeholder attrs.
fn read_result_from_cache(
    path: &omnifs_core::path::Path,
    payload: FilePayload,
    attrs: Option<&FileAttrsCache>,
) -> Result<ReadResult> {
    let content_len = payload.content.len();
    let attrs = attrs
        .map(|attrs| {
            attrs
                .learned_complete_content_attrs(content_len)
                .map_err(|error| {
                    TreeError::internal(format!(
                        "cached file payload for {path} contradicts file attrs: {error}"
                    ))
                })
        })
        .transpose()?;
    Ok(ReadResult::Bytes {
        data: payload.content,
        attrs,
        content_type: payload.content_type,
    })
}
