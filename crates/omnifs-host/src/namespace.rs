use crate::agents_doc::{AGENTS_MD_CONTENT_TYPE, AGENTS_MD_NAME};
use crate::clock::now_millis;
use crate::inflight::{Acquired, share_outcome, unshare_outcome};
use crate::materialize::{LookupEntry, LookupOutcome, Materializer};
use crate::object_id::ObjectId;
use crate::protocol_path::parse_protocol_path;
use crate::runtime::Result;
use crate::{Error, Namespace, Op};
use omnifs_cache::RecordKind;
use omnifs_core::path::{Path, Segment};
use omnifs_core::view::{AttrPayload, ByteSource, EntryMeta, FileAttrsCache, FileSize, Stability};
use omnifs_inspector::TraceId;
use omnifs_wit::provider::types as wit_types;

impl Namespace<'_> {
    pub async fn lookup_child(
        &self,
        parent_path: &str,
        name: &str,
        fuse_trace: Option<TraceId>,
    ) -> Result<LookupOutcome> {
        let parent_path = parse_protocol_path(parent_path)?;
        let name =
            Segment::try_from(name).map_err(|error| Error::ProviderProtocol(error.to_string()))?;
        let child_path = parent_path.join_segment(&name);
        // Collision-first: only synthesize the mount-root `AGENTS.md` when the
        // provider does not resolve it. A provider that projects its own
        // `/AGENTS.md` always wins below.
        let synthesize_agents_md = Self::is_root_agents_md(&parent_path, &name);
        let now = now_millis();
        if self.runtime.cache.negative_for(&child_path, now).is_some() {
            if synthesize_agents_md {
                return Ok(self.agents_md_lookup_outcome(&child_path));
            }
            return Ok(LookupOutcome::NotFound);
        }
        let op_gen = self.runtime.current_generation();
        let op = Op::LookupChild {
            parent_path: parent_path.clone(),
            name,
        };
        let child_key = child_path.as_str();
        let result = self
            .coalesced(child_key, || self.runtime.run_op(op.clone(), fuse_trace))
            .await?;

        match result {
            wit_types::OpResult::LookupChild(result) => {
                let outcome = Materializer::new(&self.runtime.cache).lookup(
                    &parent_path,
                    &child_path,
                    result,
                    op_gen,
                    now_millis(),
                );
                if synthesize_agents_md && matches!(outcome, LookupOutcome::NotFound) {
                    return Ok(self.agents_md_lookup_outcome(&child_path));
                }
                Ok(outcome)
            },
            wit_types::OpResult::Error(error) => Err(Error::ProviderError(error)),
            result => Err(Error::unexpected_op_result(op, result)),
        }
    }

    /// The synthetic mount-root `AGENTS.md` as a lookup outcome.
    fn agents_md_lookup_outcome(&self, child_path: &Path) -> LookupOutcome {
        LookupOutcome::Entry(LookupEntry::synthetic(
            child_path.clone(),
            EntryMeta::file(self.agents_doc_attrs()),
        ))
    }

    pub async fn list_children(
        &self,
        path: &str,
        cached_validator: Option<String>,
        cursor: Option<wit_types::Cursor>,
        fuse_trace: Option<TraceId>,
    ) -> Result<wit_types::ListChildrenResult> {
        let path = parse_protocol_path(path)?;
        let is_continuation = cursor.is_some();
        let op_gen = self.runtime.current_generation();
        let op = Op::ListChildren {
            path: path.clone(),
            cached_validator,
            cursor,
        };
        let path_key = path.as_str();
        let result = if is_continuation {
            self.runtime.run_op(op.clone(), fuse_trace).await?
        } else {
            self.coalesced(path_key, || self.runtime.run_op(op.clone(), fuse_trace))
                .await?
        };

        if let wit_types::OpResult::ListChildren(wit_types::ListChildrenResult::Entries(
            ref listing,
        )) = result
        {
            let m = Materializer::new(&self.runtime.cache);
            if is_continuation {
                m.apply_continuation_projection(&path, &listing.entries, op_gen);
            } else {
                m.apply_listing_projection(&path, listing, op_gen);
            }
        }

        match result {
            wit_types::OpResult::ListChildren(mut result) => {
                // Inject the synthetic mount-root `AGENTS.md` only at the root,
                // only into a concrete listing, and only when the provider did
                // not already enumerate an entry of that name (collision-first).
                if path.is_root()
                    && let wit_types::ListChildrenResult::Entries(listing) = &mut result
                    && !listing
                        .entries
                        .iter()
                        .any(|entry| entry.name == AGENTS_MD_NAME)
                {
                    listing.entries.push(self.agents_doc_dir_entry());
                }
                Ok(result)
            },
            wit_types::OpResult::Error(error) => Err(Error::ProviderError(error)),
            result => Err(Error::unexpected_op_result(op, result)),
        }
    }

    pub async fn read_file(
        &self,
        path: &str,
        content_type: String,
        fuse_trace: Option<TraceId>,
    ) -> Result<wit_types::ReadFileResult> {
        let path = parse_protocol_path(path)?;
        // Collision-first: a provider that projects its own `/AGENTS.md` wins;
        // the synthetic doc is served only when the provider yields not-found.
        let synthesize_agents_md = path
            .parent_and_name()
            .is_some_and(|(parent, name)| parent.is_root() && name == AGENTS_MD_NAME);
        let now = now_millis();
        if self.runtime.cache.negative_for(&path, now).is_some() {
            if synthesize_agents_md {
                return Ok(self.agents_doc_read_result());
            }
            return Err(enoent(path.as_str()));
        }

        // Single cache lookup: derive both the warm_id (for coalescing key and
        // live check) and the CanonicalInput (byte buffer for the provider).
        let (warm_id, cached_canonical) = match self.runtime.cache.cached_canonical_for(&path) {
            Some((host_id, bytes, validator)) => {
                let canonical = ObjectId::from_bytes(host_id.clone()).to_wit().map(|id| {
                    wit_types::CanonicalInput {
                        id,
                        validator,
                        bytes,
                    }
                });
                (Some(host_id), canonical)
            },
            None => (None, None),
        };

        let live = warm_id
            .as_ref()
            .and_then(|_| leaf_stability(self, &path))
            .is_some_and(|s| s == Stability::Live);

        // Cheap op for the error arm: no byte buffer, same path/content_type shape.
        let op_for_error = Op::ReadFile {
            path: path.clone(),
            content_type: content_type.clone(),
            cached_canonical: None,
        };
        let op = Op::ReadFile {
            path: path.clone(),
            content_type,
            cached_canonical,
        };

        let path_key = path.as_str();
        let result = if live {
            self.runtime.run_op(op, fuse_trace).await?
        } else if let Some(host_id) = warm_id {
            let id_key = hex::encode(&host_id);
            self.coalesced(&id_key, || self.runtime.run_op(op.clone(), fuse_trace))
                .await?
        } else {
            self.coalesced(path_key, || self.runtime.run_op(op.clone(), fuse_trace))
                .await?
        };

        match result {
            wit_types::OpResult::ReadFile(wit_types::ReadFileOutcome::Found(r)) => Ok(r),
            wit_types::OpResult::ReadFile(wit_types::ReadFileOutcome::NotFound(_)) => {
                if synthesize_agents_md {
                    return Ok(self.agents_doc_read_result());
                }
                Err(enoent(path.as_str()))
            },
            wit_types::OpResult::Error(error) => Err(Error::ProviderError(error)),
            result => Err(Error::unexpected_op_result(op_for_error, result)),
        }
    }

    pub async fn open_file(&self, path: &str) -> Result<wit_types::OpenFileResult> {
        let path = parse_protocol_path(path)?;
        let op = Op::OpenFile { path };
        let result = self.runtime.run_op(op.clone(), None).await?;

        match result {
            wit_types::OpResult::OpenFile(result) => Ok(result),
            wit_types::OpResult::Error(error) => Err(Error::ProviderError(error)),
            result => Err(Error::unexpected_op_result(op, result)),
        }
    }

    pub async fn read_chunk(
        &self,
        handle: u64,
        offset: u64,
        length: u32,
    ) -> Result<wit_types::ReadChunkResult> {
        let op = Op::ReadChunk {
            handle,
            offset,
            length,
        };
        let result = self.runtime.run_op(op.clone(), None).await?;

        match result {
            wit_types::OpResult::ReadChunk(result) => Ok(result),
            wit_types::OpResult::Error(error) => Err(Error::ProviderError(error)),
            result => Err(Error::unexpected_op_result(op, result)),
        }
    }

    async fn coalesced<F, Fu>(&self, key: &str, op: F) -> Result<wit_types::OpResult>
    where
        F: Fn() -> Fu,
        Fu: std::future::Future<Output = Result<wit_types::OpResult>>,
    {
        loop {
            match self.runtime.inflight.acquire(key) {
                Acquired::Leader { guard } => {
                    let result = op().await;
                    guard.complete(share_outcome(&result));
                    return result;
                },
                Acquired::ExactMatch { mut rx } => {
                    if let Ok(outcome) = rx.recv().await {
                        return unshare_outcome(outcome, Error::ProviderProtocol);
                    }
                },
                Acquired::AncestorWait { mut rx } => {
                    let _ = rx.recv().await;
                },
            }
        }
    }
}

fn leaf_stability(ns: &Namespace<'_>, path: &Path) -> Option<Stability> {
    ns.runtime
        .cache_get(path, RecordKind::Attr, None)
        .and_then(|record| AttrPayload::deserialize(&record.payload))
        .and_then(|attr| attr.meta.attrs.as_ref().map(|a| a.stability))
}

fn enoent(path: &str) -> Error {
    Error::ProviderError(wit_types::ProviderError {
        kind: wit_types::ErrorKind::NotFound,
        message: format!("no such file: {path}"),
        retryable: false,
        retry_after: None,
    })
}

// ---------------------------------------------------------------------------
// Synthetic mount-root `AGENTS.md`.
//
// The host projects a read-only `AGENTS.md` at every mount root, rendered once
// at `Runtime` construction from `provider-info` + the static manifest. The
// injection lives here in the shared host layer so both the FUSE and NFS
// frontends inherit it; no frontend-specific code is involved.
//
// Collision safety: the provider always resolves first. The synthetic leaf is
// only materialized when the provider itself yields not-found for `/AGENTS.md`
// at the root, so a provider that projects its own `/AGENTS.md` always wins and
// the listing never carries a duplicate entry.
//
// Size honesty: `lookup`/`list` advertise `runtime.agents_doc().len()` and
// `read` returns `runtime.agents_doc().bytes()`. Both read the same held buffer,
// so `stat`/`wc -c`/`head -c` always agree with `cat`.
// ---------------------------------------------------------------------------

impl Namespace<'_> {
    /// True when `parent_path` is this mount's root and `name` is the synthetic
    /// doc leaf.
    fn is_root_agents_md(parent_path: &Path, name: &Segment) -> bool {
        parent_path.is_root() && name.as_str() == AGENTS_MD_NAME
    }

    /// The exact size (in bytes) the synthetic doc advertises.
    fn agents_doc_size(&self) -> u64 {
        self.runtime.agents_doc().len()
    }

    /// File attributes for the synthetic doc. Stable/static, since it derives
    /// entirely from static provider metadata, with an exact size so
    /// `stat`/`wc -c` are honest. The bytes are served inline by `read_file`.
    fn agents_doc_attrs(&self) -> FileAttrsCache {
        FileAttrsCache {
            size: FileSize::Exact(self.agents_doc_size()),
            bytes: ByteSource::Deferred(omnifs_core::view::ReadMode::Full),
            stability: Stability::Stable,
            version_token: None,
        }
    }

    /// The synthetic doc as a listing entry (for `list_children`).
    fn agents_doc_dir_entry(&self) -> wit_types::DirEntry {
        wit_types::DirEntry {
            name: AGENTS_MD_NAME.to_string(),
            kind: wit_types::EntryKind::File(wit_types::FileOut {
                content_type: Some(AGENTS_MD_CONTENT_TYPE.to_string()),
                attrs: wit_types::FileAttrs {
                    size: wit_types::FileSize::Exact(self.agents_doc_size()),
                    stability: wit_types::Stability::Stable,
                    version_token: None,
                },
                bytes: wit_types::ByteSource::Deferred(wit_types::ReadMode::Full),
            }),
            id: None,
        }
    }

    /// The synthetic doc as a `read-file` answer: the held bytes, inline.
    fn agents_doc_read_result(&self) -> wit_types::ReadFileResult {
        wit_types::ReadFileResult {
            content_type: Some(AGENTS_MD_CONTENT_TYPE.to_string()),
            attrs: wit_types::FileAttrs {
                size: wit_types::FileSize::Exact(self.agents_doc_size()),
                stability: wit_types::Stability::Stable,
                version_token: None,
            },
            bytes: wit_types::ByteSource::Inline(self.runtime.agents_doc().bytes().to_vec()),
        }
    }
}
