//! Internal whole-file read result types and provider-read execution.
//!
//! This is the renderer-neutral read policy shared by FUSE and NFS: the cache
//! cascade, the per-mount `op_gen` write fence, the canonical-not-copied hybrid,
//! and learned-size promotion. It is fully async (no `block_on`): the renderer
//! drives the internal read path from its own executor and turns the neutral
//! `ReadResult` into kernel/protocol identity + reply encoding.

use crate::cache::{
    FactPayload, MountResources, ProjectionTransition, Record as CacheRecord, RecordKind,
    RecordWrite,
};
use crate::clock::now_millis;
use crate::ops::namespace::{ReadBytes, ReadOutcome};
use crate::pagination::NextPageOutcome;
use crate::render::MATERIALIZE_MAX_BYTES;
use crate::view as view_types;
use crate::view::{AttrPayload, EntryMeta, FileAttrsCache, FilePayload, LookupPayload};
use crate::{EngineError, Runtime};
use tracing::warn;

use super::error::{Result, TreeError};
use super::node::{Node, PaginationControl, SyntheticContent};
use crate::{RequestCtx, TreeNamespace};
use omnifs_api::events::CacheKind;

/// Result of the internal read path. A host-tree node is read via
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
}

/// One ranged chunk from a `RangedHandle`. `learned_attrs` is `Some` on an
/// EOF-short read when an exact size was learned, so the renderer promotes
/// st_size.
#[derive(Debug, Clone)]
pub struct Chunk {
    pub bytes: Vec<u8>,
    pub eof: bool,
    pub learned_attrs: Option<FileAttrsCache>,
}

pub(crate) struct FileAttrStore<'a> {
    resources: &'a MountResources,
    path: &'a omnifs_core::path::Path,
}

impl<'a> FileAttrStore<'a> {
    pub(crate) fn new(resources: &'a MountResources, path: &'a omnifs_core::path::Path) -> Self {
        Self { resources, path }
    }

    pub(crate) fn cached(
        &self,
        now_millis: u64,
        respect_expiry: bool,
    ) -> Result<Option<FileAttrsCache>> {
        let lookup = if respect_expiry {
            self.resources
                .view_get(self.path, RecordKind::Lookup, None, now_millis)
        } else {
            self.resources
                .cache_get(self.path, RecordKind::Lookup, None)
        }
        .map_err(|error| TreeError::internal(error.to_string()))?;
        if let Some(record) = lookup {
            let payload: LookupPayload = postcard::from_bytes(&record.payload)
                .map_err(|error| TreeError::internal(error.to_string()))?;
            if let LookupPayload::Positive(meta) = payload
                && let Some(attrs) = meta.into_attrs()
            {
                return Ok(Some(attrs));
            }
        }

        let attrs = if respect_expiry {
            self.resources
                .view_get(self.path, RecordKind::Attr, None, now_millis)
        } else {
            self.resources.cache_get(self.path, RecordKind::Attr, None)
        }
        .map_err(|error| TreeError::internal(error.to_string()))?
        .map(|record| {
            postcard::from_bytes::<AttrPayload>(&record.payload)
                .map_err(|error| TreeError::internal(error.to_string()))
                .and_then(|payload| Ok(payload.meta.into_attrs()))
        })
        .transpose()?
        .flatten();
        Ok(attrs)
    }

    pub(crate) fn publish(&self, attrs: FileAttrsCache, captured_epoch: u64) -> Result<()> {
        let meta = EntryMeta::file(attrs);
        self.resources
            .publish(
                ProjectionTransition {
                    records: vec![
                        RecordWrite {
                            path: self.path.clone(),
                            aux: None,
                            fact: FactPayload::Lookup(LookupPayload::Positive(meta.clone())),
                        },
                        RecordWrite {
                            path: self.path.clone(),
                            aux: None,
                            fact: FactPayload::Attr(AttrPayload { meta }),
                        },
                    ],
                    ..ProjectionTransition::default()
                },
                captured_epoch,
            )
            .map_err(|error| TreeError::internal(error.to_string()))?;
        Ok(())
    }
}

impl TreeNamespace {
    /// Whole-file read. Owns the shared read cache cascade (exact-0
    /// short-circuit, mem hit, durable view hit, backing-fs read), then on a
    /// view miss the cold provider render
    /// fenced by the per-mount `op_gen`, with the canonical-not-copied hybrid
    /// and learned-size promotion.
    ///
    /// The renderer still owns its kernel-side handle caches (the per-`fh` whole
    /// buffer FUSE keeps, the inode size promotion) and kernel offset/size
    /// slicing; the internal read path returns the whole rendered file.
    pub(crate) async fn read(&self, node: &Node, _ctx: &RequestCtx) -> Result<ReadResult> {
        // A host-synthesized node (a mount-root ignore file or a `@next`/`@all`
        // pagination control) is served by `Tree`, never the provider. The
        // renderer materializes the result into a per-handle buffer so a partial
        // or repeated read never re-runs the (mutating) control action.
        if let Some(synthetic) = node.synthetic_kind() {
            return self.read_synthetic(node, &synthetic.content).await;
        }

        // A host-tree node is served by the engine from the retained capability
        // dir; `Tree` hands the path back without a provider round trip.
        if node.host().is_some() {
            return Err(TreeError::is_directory(node.path().as_str()));
        }

        let entry = self.entry_for(node.mount())?;
        let resources = entry.resources();
        let offline = entry.runtime().is_none();
        let path = node.path();
        let captured_epoch = resources.current_epoch();
        let attr_store = FileAttrStore::new(resources, path);
        let now = now_millis();
        let expired = !offline
            && resources
                .view_expired(path, now)
                .map_err(|error| TreeError::internal(error.to_string()))?;
        let projected_attrs = attr_store
            .cached(now, !offline)?
            .or_else(|| (!expired).then(|| node.attrs().cloned()).flatten());
        let attrs = projected_attrs.as_ref();
        enforce_declared_materialize_cap(path, attrs)?;

        // Exact-0 short-circuit: a file the projection sizes at exactly zero is
        // empty without any provider call.
        if let Some(attrs) = attrs
            && matches!(attrs.size(), view_types::FileSize::Exact(0))
            && (!offline || !matches!(attrs.byte_source(), view_types::ByteSource::Deferred(_)))
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
            enforce_observed_materialize_cap(path, data.len())?;
            let attrs = attrs
                .learned_complete_content_attrs(data.len())
                .map_err(|error| {
                    TreeError::internal(format!(
                        "inline projection for {path} contradicts file attrs: {error}"
                    ))
                })?;
            if !offline {
                attr_store.publish(attrs.clone(), captured_epoch)?;
            }
            return Ok(ReadResult::Bytes {
                data,
                attrs: Some(attrs),
                content_type: None,
            });
        }

        if let Some(attrs) = attrs
            && matches!(attrs.byte_source(), view_types::ByteSource::Canonical)
        {
            if let Some(canonical) = resources
                .cached_canonical_for(path)
                .map_err(|error| TreeError::internal(error.to_string()))?
            {
                enforce_observed_materialize_cap(path, canonical.bytes.len())?;
                let attrs = attrs
                    .learned_complete_content_attrs(canonical.bytes.len())
                    .map_err(|error| {
                        TreeError::internal(format!(
                            "canonical projection for {path} contradicts its body: {error}"
                        ))
                    })?;
                return Ok(ReadResult::Bytes {
                    data: canonical.bytes,
                    attrs: Some(attrs),
                    content_type: None,
                });
            }
        }

        let durable_aux = attrs.and_then(FileAttrsCache::durable_cache_aux);

        // Read cache cascade: mem (the FUSE pagination/in-memory tier), then the
        // durable view cache. Both are keyed by the durable aux and validated
        // against the projected attrs. A hit serves the cached bytes and keeps
        // the node's projected (already size-learned) attrs.
        if let Some(aux) = durable_aux.clone() {
            if let Some(record) = resources.memory_get(path, RecordKind::File, aux.as_deref())
                && let Some(payload) = file_payload_for_attrs(&record, attrs)
            {
                crate::inspector::cache_event(CacheKind::FileHit);
                return read_result_from_cache(path, payload, attrs);
            }
            let durable = if offline {
                resources.cache_get(path, RecordKind::File, aux.as_deref())
            } else {
                resources.view_get(path, RecordKind::File, aux.as_deref(), now)
            }
            .map_err(|error| TreeError::internal(error.to_string()))?;
            if let Some(record) = durable
                && let Some(payload) = file_payload_for_attrs(&record, attrs)
            {
                crate::inspector::cache_event(CacheKind::FileHit);
                return read_result_from_cache(path, payload, attrs);
            }
        }

        if offline
            && let Some(attrs) = attrs
            && let view_types::ByteSource::Body(body) = attrs.byte_source()
        {
            let view_types::FileSize::Exact(length) = attrs.size() else {
                return Err(TreeError::internal(
                    "validated durable body metadata lost its exact length",
                ));
            };
            let data = resources
                .read_body(body, length)
                .map_err(|error| TreeError::internal(error.to_string()))?;
            enforce_observed_materialize_cap(path, data.len())?;
            return Ok(ReadResult::Bytes {
                data,
                attrs: Some(attrs.clone()),
                content_type: None,
            });
        }

        if offline {
            return Err(TreeError::offline_miss(format!(
                "offline read has no complete body for {path}"
            )));
        }

        let runtime = entry.runtime().expect("online entry has a runtime");

        // Cold miss. Derive the content type the host echoes into `read-file`:
        // the path's representation suffix wins, else octet-stream (the
        // SDK-supplied content type is unavailable on a cold read).
        let content_type = node.path().content_type_mime(None).to_string();

        // Capture the invalidation epoch BEFORE awaiting the render so the result can be
        // fenced against an invalidation that lands mid-read.
        crate::inspector::cache_event(CacheKind::FileMiss);
        let result = match runtime.read_file(path, content_type, captured_epoch).await {
            Ok(result) => result,
            Err(EngineError::ProviderError(error)) => {
                warn!(
                    path = %path,
                    kind = ?error.kind,
                    retryable = error.retryable,
                    message = error.message,
                    "provider returned typed error for read_file"
                );
                return Err(EngineError::ProviderError(error).into());
            },
            Err(error) => {
                warn!(path = %path, error = %error, "read_file runtime error");
                return Err(error.into());
            },
        };

        finish_read(&runtime, path, result)
    }

    /// Serve a host-synthesized node. A `Fixed` synthetic (a mount-root ignore
    /// file) returns its static bytes with the node's projected attrs; a
    /// `PaginationControl` runs the accumulating pagination action over the
    /// parent directory, invalidates the parent's cached dirents so a later
    /// listing reflects the grown feed, and returns a one-line status with a
    /// learned exact size so `cat` reads the whole message.
    async fn read_synthetic(&self, node: &Node, content: &SyntheticContent) -> Result<ReadResult> {
        match content {
            SyntheticContent::Fixed(bytes) => Ok(ReadResult::Bytes {
                data: materialized_bytes(node.path(), bytes.clone())?,
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
                    PaginationControl::All => runtime.paginate_all(&parent).await,
                    PaginationControl::Next => match runtime.paginate_next(&parent).await {
                        NextPageOutcome::Loaded { added, more } => format!(
                            "loaded +{added} entries; {}\n",
                            if more { "more available" } else { "complete" }
                        ),
                        NextPageOutcome::NoMore => "no more pages\n".to_string(),
                        NextPageOutcome::Error(message) => message,
                    },
                };
                // The action grew (or exhausted) the parent's accumulated
                // dirents; drop the parent's mem listing so a later browse
                // re-reads the stored record. The kernel re-list notify stays
                // renderer-side (driven from the InvalidationReport).
                runtime
                    .resources
                    .memory_invalidate(&parent, RecordKind::Dirents, None);
                let bytes = status.into_bytes();
                enforce_observed_materialize_cap(node.path(), bytes.len())?;
                let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                Ok(ReadResult::Bytes {
                    data: bytes,
                    attrs: Some(super::synthetic::control_read_attrs(len)),
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
    result: ReadOutcome,
) -> Result<ReadResult> {
    let Some((data, result_attrs, content_type)) = resolve_read_payload(runtime, path, result)?
    else {
        return Err(TreeError::internal(format!(
            "read for {path} could not resolve its byte source"
        )));
    };
    enforce_observed_materialize_cap(path, data.len())?;

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
    Ok(ReadResult::Bytes {
        data,
        attrs: Some(attrs_cache),
        content_type,
    })
}

/// Materialize a `read-file` terminal into bytes. Inline content travels in the
/// WIT; blob content is pulled from the host blob cache; `canonical` is served
/// from the anchor-keyed canonical store without copying across the WIT.
/// Returns `None` when the byte source can't be resolved
/// (logged at warn for diagnostics).
fn resolve_read_payload(
    runtime: &Runtime,
    path: &omnifs_core::path::Path,
    result: ReadOutcome,
) -> Result<Option<(Vec<u8>, FileAttrsCache, Option<String>)>> {
    let attrs = result.attrs;
    let content_type = result.content_type;
    match result.bytes {
        ReadBytes::Inline(bytes) => Ok(Some((bytes, attrs, content_type))),
        ReadBytes::Body(body) => match runtime.read_blob_full(
            body,
            match attrs.size() {
                view_types::FileSize::Exact(length) => Some(length),
                view_types::FileSize::NonZero | view_types::FileSize::Unknown => None,
            },
        ) {
            Ok(bytes) => Ok(Some((bytes, attrs, content_type))),
            Err(e) => {
                warn!(path = %path, error = %e, "blob-backed read failed");
                Ok(None)
            },
        },
        ReadBytes::Canonical => {
            if let Some(bytes) = runtime
                .canonical_bytes_for(path)
                .map_err(|error| TreeError::internal(error.to_string()))?
            {
                Ok(Some((bytes, attrs, content_type)))
            } else {
                warn!(
                    path = %path,
                    "read answered byte-source::canonical but no canonical covers the path"
                );
                Ok(None)
            }
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
    enforce_observed_materialize_cap(path, content_len)?;
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

pub(crate) fn enforce_declared_materialize_cap(
    path: &omnifs_core::path::Path,
    attrs: Option<&FileAttrsCache>,
) -> Result<()> {
    let Some(attrs) = attrs else {
        return Ok(());
    };
    if !attrs.is_deferred_full() {
        return Ok(());
    }
    let view_types::FileSize::Exact(size) = attrs.size() else {
        return Ok(());
    };
    if size <= MATERIALIZE_MAX_BYTES {
        return Ok(());
    }
    Err(TreeError::too_large(format!(
        "full read for {path} declares {size} bytes, above materialize cap {MATERIALIZE_MAX_BYTES}"
    )))
}

fn enforce_observed_materialize_cap(path: &omnifs_core::path::Path, size: usize) -> Result<()> {
    let size = u64::try_from(size).unwrap_or(u64::MAX);
    if size <= MATERIALIZE_MAX_BYTES {
        return Ok(());
    }
    Err(TreeError::too_large(format!(
        "full read for {path} materialized {size} bytes, above cap {MATERIALIZE_MAX_BYTES}"
    )))
}

fn materialized_bytes(path: &omnifs_core::path::Path, data: Vec<u8>) -> Result<Vec<u8>> {
    enforce_observed_materialize_cap(path, data.len())?;
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::error::TreeErrorKind;

    #[test]
    fn declared_materialize_cap_is_inclusive() {
        let path = omnifs_core::path::Path::parse("/message").expect("valid path");
        let at_cap = FileAttrsCache::deferred(
            view_types::FileSize::Exact(MATERIALIZE_MAX_BYTES),
            view_types::ReadMode::Full,
            view_types::Stability::Stable,
            None,
        )
        .expect("valid capped attrs");
        assert!(enforce_declared_materialize_cap(&path, Some(&at_cap)).is_ok());

        let above_cap = FileAttrsCache::deferred(
            view_types::FileSize::Exact(MATERIALIZE_MAX_BYTES + 1),
            view_types::ReadMode::Full,
            view_types::Stability::Stable,
            None,
        )
        .expect("valid oversized attrs");
        let error = enforce_declared_materialize_cap(&path, Some(&above_cap))
            .expect_err("full materialization above the cap must be rejected");
        assert_eq!(error.kind, TreeErrorKind::TooLarge);
    }
}
