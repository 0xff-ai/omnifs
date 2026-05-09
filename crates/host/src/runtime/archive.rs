//! Mount a stored blob as a directory tree by extracting it via the
//! sandboxed Wasmtime extractor component.
//!
//! The provider issues `open-archive(blob, format, strip-prefix)`; the
//! host hands the blob and a fresh extraction directory to the
//! archive extractor tool and, on success, registers the resulting
//! directory in the shared [`TreeRegistry`]. The provider returns the
//! resulting `tree-ref`, which the host serves through the same FUSE
//! bind-mount path that already serves git clones. There are no
//! per-file callouts; extraction happens inside the WASI sandbox.

use crate::runtime::blob::{BlobCache, BlobRecord};
use crate::runtime::executor::{CalloutResponse, ErrorKind};
use crate::runtime::sandbox::tree_cache::{MaterializeError, MaterializedTree, TreeMaterializer};
use crate::runtime::tools::archive::{
    self as archive_tool, ArchiveExtractorComponent, ArchiveFormat, ExtractError, ExtractStats,
    ExtractorLimits,
};
use crate::runtime::tree_registry::TreeRegistry;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
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
/// extraction is delegated to the sandboxed
/// [`ArchiveExtractorComponent`].
///
/// Cached extraction directories are keyed by the full `(blob-id,
/// format, strip-prefix)` view and published with a temporary directory
/// rename, so a registered `tree-ref` never points at a partial tree.
pub(crate) struct ArchiveExecutor {
    cache: Arc<BlobCache>,
    materializer: TreeMaterializer<ExtractKey>,
    extractor: Arc<ArchiveExtractorComponent>,
    limits: ExtractorLimits,
    /// Defense-in-depth check that walks the extracted directory and
    /// confirms its byte total matches what the sandbox reported. Off
    /// by default; the WASI preopen capability is what enforces the
    /// scope; the audit only catches a counter bug in the host-shipped
    /// wasm. Leaving it off keeps it from costing real time on large
    /// source trees (Chromium-class).
    audit_bytes: bool,
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
        extractor: Arc<ArchiveExtractorComponent>,
    ) -> Self {
        Self {
            cache,
            materializer: TreeMaterializer::new(extract_root, trees),
            extractor,
            limits: archive_tool::DEFAULT_LIMITS,
            audit_bytes: false,
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

        let Some(record) = self.cache.lookup(blob_id) else {
            return error(
                ErrorKind::NotFound,
                format!("blob {blob_id} not found"),
                false,
            );
        };

        let result = self
            .materializer
            .materialize(&key, extraction_dir_name(&key), |tmp| {
                self.extract_to(tmp, &key, &record, strip_prefix.as_deref())
            });
        let tree = match result {
            Ok(MaterializedTree::Cached { tree }) => tree,
            Ok(MaterializedTree::Fresh {
                tree,
                output: stats,
            }) => {
                tracing::debug!(
                    blob = blob_id,
                    entries = stats.entries,
                    bytes = stats.bytes_written,
                    "archive extracted"
                );
                tree
            },
            Err(MaterializeError::Run(response)) => return response,
            Err(MaterializeError::Prepare(e)) => {
                return error(
                    ErrorKind::Internal,
                    format!("prepare archive extraction: {e}"),
                    false,
                );
            },
            Err(MaterializeError::Publish(e)) => {
                return error(
                    ErrorKind::Internal,
                    format!("publish archive extraction: {e}"),
                    false,
                );
            },
        };
        CalloutResponse::ArchiveOpened(tree)
    }

    fn extract_to(
        &self,
        tmp: &Path,
        key: &ExtractKey,
        record: &BlobRecord,
        strip_prefix: Option<&str>,
    ) -> Result<ExtractStats, CalloutResponse> {
        let stats =
            match self
                .extractor
                .extract(key.format, &record.path, tmp, strip_prefix, self.limits)
            {
                Ok(stats) => stats,
                Err(e) => {
                    tracing::warn!(blob = key.blob_id, error = %e, "archive extraction failed");
                    return Err(error(error_kind_for(&e), e.to_string(), false));
                },
            };

        if self.audit_bytes
            && let Err(e) = audit(tmp, stats)
        {
            tracing::warn!(blob = key.blob_id, error = %e, "post-extract audit failed");
            return Err(error(ErrorKind::Internal, e.to_string(), false));
        }
        Ok(stats)
    }
}

fn extraction_dir_name(key: &ExtractKey) -> String {
    let mut hasher = Sha256::new();
    hasher.update(key.blob_id.to_le_bytes());
    hasher.update(archive_format_component(key.format).as_bytes());
    match &key.strip_prefix {
        Some(strip_prefix) => {
            hasher.update([1]);
            hasher.update(strip_prefix.as_bytes());
        },
        None => hasher.update([0]),
    }
    let digest = hasher.finalize();
    format!(
        "{}-{}-{}",
        key.blob_id,
        archive_format_component(key.format),
        hex_prefix(&digest, 16)
    )
}

fn archive_format_component(format: ArchiveFormat) -> &'static str {
    match format {
        ArchiveFormat::TarGz => "targz",
        ArchiveFormat::Tar => "tar",
        ArchiveFormat::Zip => "zip",
    }
}

fn hex_prefix(bytes: &[u8], len: usize) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(len * 2);
    for byte in bytes.iter().take(len) {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
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
fn audit(dest: &Path, stats: ExtractStats) -> Result<(), ArchiveError> {
    let audited = archive_tool::audit_bytes_written(dest)?;
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
    use flate2::Compression;
    use flate2::write::GzEncoder;

    fn synthesize_targz() -> Vec<u8> {
        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        {
            let mut tar = tar::Builder::new(&mut gz);
            append_targz_file(
                &mut tar,
                "pkg-1.0/Cargo.toml",
                b"[package]\nname = \"pkg\"\nversion = \"1.0.0\"\n",
            );
            append_targz_file(
                &mut tar,
                "pkg-1.0/src/lib.rs",
                b"pub fn answer() -> u32 { 42 }\n",
            );
            tar.finish().unwrap();
        }
        gz.finish().unwrap()
    }

    fn synthesize_multi_root_targz() -> Vec<u8> {
        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        {
            let mut tar = tar::Builder::new(&mut gz);
            append_targz_file(&mut tar, "alpha/only.txt", b"alpha\n");
            append_targz_file(&mut tar, "beta/only.txt", b"beta\n");
            tar.finish().unwrap();
        }
        gz.finish().unwrap()
    }

    fn append_targz_file(
        tar: &mut tar::Builder<&mut GzEncoder<Vec<u8>>>,
        path: &str,
        bytes: &[u8],
    ) {
        let mut header = tar::Header::new_gnu();
        header.set_path(path).unwrap();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append(&header, bytes).unwrap();
    }

    #[test]
    fn extraction_dir_name_changes_with_full_key() {
        let base = ExtractKey {
            blob_id: 7,
            format: ArchiveFormat::TarGz,
            strip_prefix: Some("alpha/".into()),
        };
        let different_strip = ExtractKey {
            strip_prefix: Some("beta/".into()),
            ..base.clone()
        };
        let different_format = ExtractKey {
            format: ArchiveFormat::Zip,
            ..base.clone()
        };

        assert_ne!(
            extraction_dir_name(&base),
            extraction_dir_name(&different_strip)
        );
        assert_ne!(
            extraction_dir_name(&base),
            extraction_dir_name(&different_format)
        );
    }

    fn insert_archive_blob(cache: &BlobCache, blob_path: PathBuf, archive_bytes: &[u8]) {
        std::fs::write(&blob_path, archive_bytes).unwrap();
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
    }

    #[test]
    fn open_archive_returns_tree_ref_resolving_to_extracted_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let blob_cache_dir = tmp.path().join("blobs");
        let archive_root = tmp.path().join("archives");
        std::fs::create_dir_all(&blob_cache_dir).unwrap();
        std::fs::create_dir_all(&archive_root).unwrap();

        let cache = Arc::new(BlobCache::new(blob_cache_dir.clone()));

        let blob_path = blob_cache_dir.join("pkg-1.0.crate");
        insert_archive_blob(&cache, blob_path, &synthesize_targz());

        let trees = Arc::new(TreeRegistry::new());
        let extractor = Arc::new(ArchiveExtractorComponent::new().expect("build extractor"));
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

    #[test]
    fn same_blob_with_distinct_strip_prefixes_keeps_stable_tree_refs() {
        let tmp = tempfile::tempdir().unwrap();
        let blob_cache_dir = tmp.path().join("blobs");
        let archive_root = tmp.path().join("archives");
        std::fs::create_dir_all(&blob_cache_dir).unwrap();
        std::fs::create_dir_all(&archive_root).unwrap();

        let cache = Arc::new(BlobCache::new(blob_cache_dir.clone()));
        insert_archive_blob(
            &cache,
            blob_cache_dir.join("multi-root.crate"),
            &synthesize_multi_root_targz(),
        );

        let trees = Arc::new(TreeRegistry::new());
        let extractor = Arc::new(ArchiveExtractorComponent::new().expect("build extractor"));
        let executor =
            ArchiveExecutor::new(cache, trees.clone(), archive_root, extractor).with_audit();

        let alpha = match executor.open_archive(7, ArchiveFormat::TarGz, Some("alpha/")) {
            CalloutResponse::ArchiveOpened(tree) => tree,
            other => panic!("unexpected alpha response: {other:?}"),
        };
        let alpha_root = trees.resolve(alpha).expect("alpha tree-ref resolves");
        assert_eq!(
            std::fs::read_to_string(alpha_root.join("only.txt")).unwrap(),
            "alpha\n"
        );

        let beta = match executor.open_archive(7, ArchiveFormat::TarGz, Some("beta/")) {
            CalloutResponse::ArchiveOpened(tree) => tree,
            other => panic!("unexpected beta response: {other:?}"),
        };
        let beta_root = trees.resolve(beta).expect("beta tree-ref resolves");
        assert_eq!(
            std::fs::read_to_string(beta_root.join("only.txt")).unwrap(),
            "beta\n"
        );

        assert_ne!(alpha_root, beta_root);
        assert_eq!(
            std::fs::read_to_string(alpha_root.join("only.txt")).unwrap(),
            "alpha\n"
        );
        assert_eq!(
            std::fs::read_to_string(beta_root.join("only.txt")).unwrap(),
            "beta\n"
        );
    }

    #[test]
    fn resolved_archive_tree_ref_supports_nested_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        let blob_cache_dir = tmp.path().join("blobs");
        let archive_root = tmp.path().join("archives");
        std::fs::create_dir_all(&blob_cache_dir).unwrap();
        std::fs::create_dir_all(&archive_root).unwrap();

        let cache = Arc::new(BlobCache::new(blob_cache_dir.clone()));
        insert_archive_blob(
            &cache,
            blob_cache_dir.join("pkg-1.0.crate"),
            &synthesize_targz(),
        );

        let trees = Arc::new(TreeRegistry::new());
        let extractor = Arc::new(ArchiveExtractorComponent::new().expect("build extractor"));
        let executor =
            ArchiveExecutor::new(cache, trees.clone(), archive_root, extractor).with_audit();

        let tree = match executor.open_archive(7, ArchiveFormat::TarGz, Some("pkg-1.0/")) {
            CalloutResponse::ArchiveOpened(tree) => tree,
            other => panic!("unexpected response: {other:?}"),
        };
        let root = trees.resolve(tree).expect("tree-ref resolves");
        assert!(root.join("src").is_dir());
        assert_eq!(
            std::fs::read_to_string(root.join("src/lib.rs")).unwrap(),
            "pub fn answer() -> u32 { 42 }\n"
        );
    }

    #[test]
    fn constructor_sweeps_stale_temp_extraction_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let blob_cache_dir = tmp.path().join("blobs");
        let archive_root = tmp.path().join("archives");
        std::fs::create_dir_all(&blob_cache_dir).unwrap();
        std::fs::create_dir_all(&archive_root).unwrap();
        let stale = archive_root.join(".7-targz-deadbeef.tmp.123.456");
        std::fs::create_dir_all(&stale).unwrap();
        let keep = archive_root.join("7-targz-live");
        std::fs::create_dir_all(&keep).unwrap();

        let cache = Arc::new(BlobCache::new(blob_cache_dir));
        let trees = Arc::new(TreeRegistry::new());
        let extractor = Arc::new(ArchiveExtractorComponent::new().expect("build extractor"));
        let _executor = ArchiveExecutor::new(cache, trees, archive_root, extractor);

        assert!(!stale.exists());
        assert!(keep.exists());
    }
}
