//! Mount a stored blob as a directory tree by extracting it via the
//! sandboxed Wasmtime extractor component.
//!
//! The provider issues `open-archive(blob, format, strip-prefix)`; the
//! host hands the blob and a fresh extraction directory to the
//! extractor component (see [`crate::runtime::wasm_extractor`]) and,
//! on success, registers the resulting directory in the shared
//! [`TreeRegistry`]. The provider returns the resulting `tree-ref`,
//! which the host serves through the same FUSE bind-mount path that
//! already serves git clones — no per-file callouts, all extraction
//! happens inside the WASI sandbox.

use crate::runtime::blob::BlobCache;
use crate::runtime::executor::{CalloutResponse, ErrorKind};
use crate::runtime::tree_registry::TreeRegistry;
use crate::runtime::wasm_extractor::{
    self, ArchiveFormat, ExtractError, ExtractStats, ExtractorLimits, WasmExtractor,
};
use dashmap::DashMap;
use parking_lot::Mutex;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub(crate) enum ArchiveError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("extractor sandbox: {0}")]
    Extractor(#[from] ExtractError),
    #[error("audit: extractor reported {reported} bytes but disk holds {audited}")]
    AuditMismatch { reported: u64, audited: u64 },
}

/// Per-provider archive extractor. Owns the on-disk extraction root and
/// shares a [`TreeRegistry`] with the git executor so a returned
/// `tree-ref` resolves identically regardless of source. All actual
/// extraction is delegated to the sandboxed [`WasmExtractor`].
pub(crate) struct ArchiveExecutor {
    cache: Arc<BlobCache>,
    trees: Arc<TreeRegistry>,
    extract_root: PathBuf,
    extractor: Arc<WasmExtractor>,
    limits: ExtractorLimits,
    /// Defense-in-depth check that walks the extracted directory and
    /// confirms its byte total matches what the sandbox reported. Off
    /// by default — the WASI preopen capability is what enforces the
    /// scope; the audit only catches a counter bug in the host-shipped
    /// wasm. Off-by-default keeps it from costing real time on large
    /// source trees (Chromium-class).
    audit_bytes: bool,
    /// `(blob-id, format, strip-prefix)` → cached `tree-ref`. An
    /// archive is extracted at most once per `(blob, params)` triple
    /// even across concurrent open-archive calls.
    extracted: DashMap<ExtractKey, u64>,
    /// Per-key lock to coalesce concurrent extractions.
    locks: DashMap<ExtractKey, Arc<Mutex<()>>>,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct ExtractKey {
    blob_id: u64,
    format: ArchiveFormat,
    strip_prefix: Option<String>,
}

impl ArchiveExecutor {
    pub(crate) fn new(
        cache: Arc<BlobCache>,
        trees: Arc<TreeRegistry>,
        extract_root: PathBuf,
        extractor: Arc<WasmExtractor>,
    ) -> Self {
        Self {
            cache,
            trees,
            extract_root,
            extractor,
            limits: wasm_extractor::DEFAULT_LIMITS,
            audit_bytes: false,
            extracted: DashMap::new(),
            locks: DashMap::new(),
        }
    }

    /// Enable the post-extract audit walk. Useful in tests; production
    /// callers leave it off because the WASI preopen scope is the
    /// real guarantee.
    #[cfg(test)]
    pub(crate) fn with_audit(mut self) -> Self {
        self.audit_bytes = true;
        self
    }

    pub(crate) fn open_archive(
        &self,
        blob_id: u64,
        format: ArchiveFormat,
        strip_prefix: Option<&str>,
    ) -> CalloutResponse {
        let strip_prefix = strip_prefix.filter(|s| !s.is_empty()).map(str::to_string);
        let key = ExtractKey {
            blob_id,
            format,
            strip_prefix: strip_prefix.clone(),
        };

        // Coalesce concurrent extractions of the same (blob, params).
        let lock = self
            .locks
            .entry(key.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _guard = lock.lock();

        // Idempotent: the registry is append-only, so a recorded
        // tree-ref stays valid for the life of the runtime.
        if let Some(id) = self.extracted.get(&key).map(|r| *r) {
            return CalloutResponse::ArchiveOpened(id);
        }

        let Some(record) = self.cache.lookup(blob_id) else {
            return error(
                ErrorKind::NotFound,
                format!("blob {blob_id} not found"),
                false,
            );
        };

        let dest = self.extract_root.join(blob_id.to_string());
        if dest.exists()
            && let Err(e) = std::fs::remove_dir_all(&dest)
        {
            return error(
                ErrorKind::Internal,
                format!("clean prior extraction at {}: {e}", dest.display()),
                false,
            );
        }
        if let Err(e) = std::fs::create_dir_all(&dest) {
            return error(
                ErrorKind::Internal,
                format!("create extract dir: {e}"),
                false,
            );
        }

        let stats = match self.extractor.extract(
            format,
            &record.path,
            &dest,
            strip_prefix.as_deref(),
            self.limits,
        ) {
            Ok(stats) => stats,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&dest);
                tracing::warn!(blob = blob_id, error = %e, "archive extraction failed");
                return error(error_kind_for(&e), e.to_string(), false);
            },
        };

        if self.audit_bytes
            && let Err(e) = audit(&dest, stats)
        {
            let _ = std::fs::remove_dir_all(&dest);
            tracing::warn!(blob = blob_id, error = %e, "post-extract audit failed");
            return error(ErrorKind::Internal, e.to_string(), false);
        }
        tracing::debug!(
            blob = blob_id,
            entries = stats.entries,
            bytes = stats.bytes_written,
            "archive extracted"
        );

        let id = self.trees.register(dest);
        self.extracted.insert(key, id);
        CalloutResponse::ArchiveOpened(id)
    }
}

fn error_kind_for(error: &ExtractError) -> ErrorKind {
    use ExtractError as E;
    match error {
        E::UnsafePath(_)
        | E::PathTooDeep(_)
        | E::PathTooLong(_)
        | E::UnsupportedEntryKind(_)
        | E::Malformed(_) => ErrorKind::InvalidInput,
        _ => ErrorKind::Internal,
    }
}

fn error(kind: ErrorKind, message: String, retryable: bool) -> CalloutResponse {
    CalloutResponse::Error {
        kind,
        message,
        retryable,
    }
}

/// Defense-in-depth audit: confirm the bytes the component reported
/// match what's actually on disk. A mismatch can't happen if the
/// sandbox's preopen-scoped writes are honest, but a bug in the
/// component would show up here.
fn audit(dest: &std::path::Path, stats: ExtractStats) -> Result<(), ArchiveError> {
    let audited = wasm_extractor::audit_bytes_written(dest)?;
    if audited != stats.bytes_written {
        return Err(ArchiveError::AuditMismatch {
            reported: stats.bytes_written,
            audited,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::blob::BlobRecord;
    use crate::runtime::test_archives::synthesize_targz;

    #[test]
    fn open_archive_returns_tree_ref_resolving_to_extracted_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let blob_cache_dir = tmp.path().join("blobs");
        let archive_root = tmp.path().join("archives");
        std::fs::create_dir_all(&blob_cache_dir).unwrap();
        std::fs::create_dir_all(&archive_root).unwrap();

        let cache = Arc::new(BlobCache::new(blob_cache_dir.clone()));

        let archive_bytes = synthesize_targz();
        let blob_path = blob_cache_dir.join("pkg-1.0.crate");
        std::fs::write(&blob_path, &archive_bytes).unwrap();
        cache.insert_for_test(
            "pkg-1.0.crate",
            BlobRecord {
                id: 7,
                path: blob_path,
                size: archive_bytes.len() as u64,
                content_type: Some("application/x-gzip".into()),
                etag: None,
                status: 200,
                response_headers: Vec::new(),
            },
        );

        let trees = Arc::new(TreeRegistry::new());
        let extractor = Arc::new(WasmExtractor::new().expect("build extractor"));
        let executor =
            ArchiveExecutor::new(cache, trees.clone(), archive_root, extractor).with_audit();

        let response = executor.open_archive(7, ArchiveFormat::TarGz, Some("pkg-1.0/"));
        let tree = match response {
            CalloutResponse::ArchiveOpened(t) => t,
            other => panic!("unexpected response: {other:?}"),
        };

        let extracted = trees.resolve(tree).expect("tree-ref resolves");
        let cargo = std::fs::read_to_string(extracted.join("Cargo.toml")).unwrap();
        assert!(cargo.contains("name = \"pkg\""));
    }
}
