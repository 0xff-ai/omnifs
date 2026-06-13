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
use omnifs_core::view::{FileAttrsCache, FilePayload};
use omnifs_host::wit_protocol::file_attrs_from_attrs;
use omnifs_host::{Error, Runtime};
use omnifs_wit::provider::types::{ByteSource, ReadFileResult};
use tracing::warn;

use crate::error::{Result, TreeError};
use crate::node::{Backing, Node};
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

impl Tree {
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
        // A treeref-backed node is served by the renderer from the real backing
        // dir; `Tree` hands the path back without a provider round trip.
        if let Backing::Subtree(dir) = node.backing() {
            return Ok(ReadResult::Backing(dir.clone()));
        }

        let runtime = self.runtime_for(node.mount())?;
        let path = node.path().as_str();
        let attrs = node.attrs();

        // Exact-0 short-circuit: a file the projection sizes at exactly zero is
        // empty without any provider call.
        if let Some(attrs) = attrs
            && matches!(attrs.size, view_types::FileSize::Exact(0))
        {
            return Ok(ReadResult::Bytes {
                data: Vec::new(),
                attrs: Some(attrs.clone()),
                content_type: None,
            });
        }

        let durable_aux = attrs.and_then(FileAttrsCache::durable_cache_aux);

        // Read cache cascade: mem (the FUSE pagination/in-memory tier), then the
        // durable view cache. Both are keyed by the durable aux and validated
        // against the projected attrs. A hit serves the cached bytes and keeps
        // the node's projected (already size-learned) attrs.
        if let Some(aux) = durable_aux.clone() {
            if let Some(record) = runtime.mem_get(path, RecordKind::File, aux.as_deref())
                && let Some(payload) = file_payload_for_attrs(&record, attrs)
            {
                return Ok(read_result_from_cache(payload, attrs));
            }
            if let Some(record) = runtime.cache_get(path, RecordKind::File, aux.as_deref())
                && let Some(payload) = file_payload_for_attrs(&record, attrs)
            {
                return Ok(read_result_from_cache(payload, attrs));
            }
        }

        // Cold miss. Derive the content type the host echoes into `read-file`:
        // the path's representation suffix wins, else octet-stream (the
        // SDK-supplied content type is unavailable on a cold read).
        let content_type = node.path().content_type_mime(None).to_string();

        // Capture the generation BEFORE awaiting the render so the result can be
        // fenced against an invalidation that lands mid-read.
        let op_gen = runtime.current_generation();
        let result = match runtime
            .namespace()
            .read_file(path, content_type, ctx.trace)
            .await
        {
            Ok(result) => result,
            Err(Error::ProviderError(error)) => {
                warn!(
                    path,
                    kind = ?error.kind,
                    retryable = error.retryable,
                    message = error.message,
                    "provider returned typed error for read_file"
                );
                return Err(Error::ProviderError(error).into());
            },
            Err(error) => {
                warn!(path, error = %error, "read_file runtime error");
                return Err(error.into());
            },
        };

        finish_read(&runtime, path, result, op_gen)
    }
}

/// Resolve a cold `read-file` terminal into bytes + learned attrs, validate, and
/// durably cache unless the canonical hybrid forbids it or the write fence
/// rejects it.
fn finish_read(
    runtime: &Runtime,
    path: &str,
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

    let attrs_cache = learned_full_read_attrs(result_attrs, data.len());
    if !full_read_matches_attrs(&attrs_cache, data.len()) {
        warn!(
            path,
            expected = ?attrs_cache.size,
            actual = data.len(),
            "provider returned bytes that contradict file attrs"
        );
        return Err(TreeError::internal(format!(
            "read for {path} returned bytes that contradict file attrs"
        )));
    }

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
    path: &str,
    attrs_cache: &FileAttrsCache,
    data: &[u8],
    content_type: Option<String>,
    op_gen: u64,
) -> Result<()> {
    let Some(aux) = attrs_cache.durable_cache_aux() else {
        return Ok(());
    };
    let payload = FilePayload::new(attrs_cache.version_token.clone(), data.to_vec())
        .with_content_type(content_type);
    let Some(payload) = payload.serialize() else {
        return Err(TreeError::internal(format!(
            "read for {path} could not serialize its file payload"
        )));
    };
    let record = CacheRecord::new(RecordKind::File, payload);
    // Drop the write if an invalidation for this path landed after the read
    // began: caching it would reinstate stale bytes.
    if runtime.write_fenced(path, op_gen) {
        return Ok(());
    }
    runtime.cache_put(path, RecordKind::File, aux.as_deref(), &record);
    Ok(())
}

/// Materialize a `read-file` terminal into bytes. Inline content travels in the
/// WIT; blob content is pulled from the host blob cache; `canonical` is served
/// from the anchor-keyed canonical store without copying across the WIT
/// (ADR-0001 §5.1). Returns `None` when the byte source can't be resolved
/// (logged at warn for diagnostics).
fn resolve_read_payload(
    runtime: &Runtime,
    path: &str,
    result: ReadFileResult,
) -> Option<(Vec<u8>, FileAttrsCache, Option<String>)> {
    let attrs = file_attrs_from_attrs(&result.attrs);
    let content_type = result.content_type;
    match result.bytes {
        ByteSource::Inline(bytes) => Some((bytes, attrs, content_type)),
        ByteSource::Blob(blob) => match runtime.read_blob_full(blob) {
            Ok(bytes) => Some((bytes, attrs, content_type)),
            Err(e) => {
                warn!(path, error = %e, "blob-backed read failed");
                None
            },
        },
        ByteSource::Canonical => {
            if let Some(bytes) = runtime.canonical_bytes_for(path) {
                Some((bytes, attrs, content_type))
            } else {
                warn!(
                    path,
                    "read answered byte-source::canonical but no canonical covers the path"
                );
                None
            }
        },
        // The validator rejects a `deferred` read answer before the read path is
        // reached; a read must produce bytes.
        ByteSource::Deferred(_) => {
            warn!(
                path,
                "read answered byte-source::deferred, which is not a valid read answer"
            );
            None
        },
    }
}

/// Promote a learned exact size into the result attrs when the read returned a
/// complete buffer and the stability permits publishing a learned size.
fn learned_full_read_attrs(attrs: FileAttrsCache, content_len: usize) -> FileAttrsCache {
    if !can_publish_learned_size(&attrs) {
        return attrs;
    }
    match attrs.size {
        view_types::FileSize::Exact(_) => attrs,
        view_types::FileSize::NonZero | view_types::FileSize::Unknown => {
            attrs.with_exact_size(u64::try_from(content_len).unwrap_or(u64::MAX))
        },
    }
}

/// Learn an exact size from a ranged EOF-short read, mirroring the whole-file
/// learned-size rule. `None` when the size is already exact or learning is
/// disallowed for the stability.
pub(crate) fn learned_ranged_eof_attrs(
    attrs: FileAttrsCache,
    eof_size: u64,
) -> Option<FileAttrsCache> {
    if !can_publish_learned_size(&attrs) {
        return None;
    }
    match attrs.size {
        view_types::FileSize::Exact(_) => None,
        view_types::FileSize::NonZero | view_types::FileSize::Unknown => {
            Some(attrs.with_exact_size(eof_size))
        },
    }
}

fn can_publish_learned_size(attrs: &FileAttrsCache) -> bool {
    match attrs.stability {
        view_types::Stability::Immutable | view_types::Stability::Mutable => true,
        view_types::Stability::Volatile => false,
    }
}

fn full_read_matches_attrs(attrs: &FileAttrsCache, content_len: usize) -> bool {
    match attrs.size {
        view_types::FileSize::Exact(size) => {
            u64::try_from(content_len).is_ok_and(|content_len| content_len == size)
        },
        view_types::FileSize::NonZero => content_len > 0,
        view_types::FileSize::Unknown => true,
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
    if matches!(attrs.stability, view_types::Stability::Mutable)
        && payload.version_token != attrs.version_token
    {
        return None;
    }
    if !full_read_matches_attrs(attrs, payload.content.len()) {
        return None;
    }
    Some(payload)
}

/// A cache hit serves the cached bytes; the renderer keeps the node's projected
/// (already size-learned) attrs, mirroring FUSE serving from the inode whose
/// size was promoted when the entry was first read.
fn read_result_from_cache(payload: FilePayload, attrs: Option<&FileAttrsCache>) -> ReadResult {
    ReadResult::Bytes {
        data: payload.content,
        attrs: attrs.cloned(),
        content_type: payload.content_type,
    }
}
