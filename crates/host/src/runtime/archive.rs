//! Mount a stored blob as a directory tree by extracting it via the
//! sandboxed Wasmtime extractor component.
//!
//! The provider issues `open-archive(blob, format, strip-prefix)`; the
//! host hands the blob and a fresh extraction directory to the
//! archive extractor tool and, on success, registers the resulting
//! directory in the shared [`TreeRefs`]. The provider returns the
//! resulting `tree-ref`, which the host serves through the same FUSE
//! bind-mount path that already serves git clones. There are no
//! per-file callouts; extraction happens inside the WASI sandbox.

#[cfg(test)]
use crate::cache::blobs::BlobMetadata;
use crate::cache::blobs::{BlobCache, BlobRecord};
use crate::omnifs::provider::types as wit_types;
use crate::runtime::sandbox::tree_cache::{
    MaterializeError, MaterializedTree, TreeKey, TreeMaterializer,
};
#[cfg(test)]
use crate::runtime::tools::archive as archive_tool;
use crate::runtime::tools::archive::{
    ArchiveExtractorComponent, ArchiveFormat, ExtractError, ExtractStats,
};
use crate::runtime::tree_refs::TreeRefs;
use crate::runtime::{callout_error, callout_internal, callout_not_found};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{debug, warn};

/// Per-provider archive extractor. Owns the on-disk extraction root and
/// shares a [`TreeRefs`] with the git executor so a returned
/// `tree-ref` resolves identically regardless of source. All actual
/// extraction is delegated to the sandboxed
/// [`ArchiveExtractorComponent`].
///
/// Cached extraction directories are keyed by the full `(cache-key,
/// format, strip-prefix)` view and published with a temporary directory
/// rename, so a registered `tree-ref` never points at a partial tree.
pub(crate) struct ArchiveExecutor {
    cache: Arc<BlobCache>,
    materializer: TreeMaterializer<ExtractKey>,
    extractor: Arc<ArchiveExtractorComponent>,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct ExtractKey {
    cache_key: String,
    format: ArchiveFormat,
    strip_prefix: Option<String>,
}

impl ExtractKey {
    fn new(
        cache_key: impl Into<String>,
        format: ArchiveFormat,
        strip_prefix: Option<String>,
    ) -> Self {
        Self {
            cache_key: cache_key.into(),
            format,
            strip_prefix,
        }
    }
}

impl TreeKey for ExtractKey {
    fn dir_name(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.cache_key.as_bytes());
        match &self.strip_prefix {
            Some(strip_prefix) => {
                hasher.update([1]);
                hasher.update(strip_prefix.as_bytes());
            },
            None => hasher.update([0]),
        }
        let digest = hasher.finalize();
        format!(
            "{}-{}",
            self.format.cache_component(),
            hex_prefix(&digest, 16),
        )
    }
}

#[derive(Debug, thiserror::Error)]
enum ArchiveError {
    #[error("{0}")]
    Extract(#[from] ExtractError),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Internal(String),
    #[error("{0}")]
    JoinFailed(String),
}

impl From<ArchiveError> for wit_types::CalloutResult {
    fn from(error: ArchiveError) -> Self {
        match error {
            ArchiveError::Extract(e) => callout_error(error_kind_for(&e), e.to_string(), false),
            ArchiveError::NotFound(msg) => callout_not_found(msg),
            ArchiveError::Internal(msg) | ArchiveError::JoinFailed(msg) => callout_internal(msg),
        }
    }
}

impl ArchiveExecutor {
    pub(crate) fn new(
        cache: Arc<BlobCache>,
        trees: Arc<TreeRefs>,
        extract_root: PathBuf,
        extractor: Arc<ArchiveExtractorComponent>,
    ) -> Self {
        Self {
            cache,
            materializer: TreeMaterializer::new(extract_root, trees),
            extractor,
        }
    }

    pub(crate) async fn open_archive(
        self: &Arc<Self>,
        blob_id: u64,
        format: ArchiveFormat,
        strip_prefix: Option<&str>,
    ) -> wit_types::CalloutResult {
        let this = Arc::clone(self);
        let strip = strip_prefix.map(str::to_string);
        let result = tokio::task::spawn_blocking(move || {
            this.open_archive_blocking(blob_id, format, strip.as_deref())
        })
        .await;
        match result {
            Ok(Ok(tree)) => {
                wit_types::CalloutResult::ArchiveOpened(wit_types::ArchiveOpened { tree })
            },
            Ok(Err(e)) => e.into(),
            Err(join_err) => {
                ArchiveError::JoinFailed(format!("extract task join: {join_err}")).into()
            },
        }
    }

    fn open_archive_blocking(
        &self,
        blob_id: u64,
        format: ArchiveFormat,
        strip_prefix: Option<&str>,
    ) -> Result<u64, ArchiveError> {
        let strip_prefix = strip_prefix.filter(|s| !s.is_empty()).map(str::to_string);
        let record = self
            .cache
            .lookup_by_id(blob_id)
            .ok_or_else(|| ArchiveError::NotFound(format!("blob {blob_id} not found")))?;
        let key = ExtractKey::new(record.cache_key.clone(), format, strip_prefix);

        let tree = match self
            .materializer
            .materialize(&key, |tmp| self.extract_to(tmp, &key, &record))
        {
            Ok(MaterializedTree::Cached { tree }) => tree,
            Ok(MaterializedTree::Fresh {
                tree,
                output: stats,
            }) => {
                debug!(
                    cache_key = key.cache_key,
                    entries = stats.entries,
                    bytes = stats.bytes_written,
                    "archive extracted"
                );
                tree
            },
            Err(MaterializeError::Run(e)) => return Err(e),
            Err(MaterializeError::Prepare(e)) => {
                return Err(ArchiveError::Internal(format!(
                    "prepare archive extraction: {e}"
                )));
            },
            Err(MaterializeError::Publish(e)) => {
                return Err(ArchiveError::Internal(format!(
                    "publish archive extraction: {e}"
                )));
            },
        };
        Ok(tree)
    }

    fn extract_to(
        &self,
        tmp: &Path,
        key: &ExtractKey,
        record: &BlobRecord,
    ) -> Result<ExtractStats, ArchiveError> {
        let blob_path = self.cache.blob_path(&record.cache_key);
        let stats =
            match self
                .extractor
                .extract(key.format, &blob_path, tmp, key.strip_prefix.as_deref())
            {
                Ok(stats) => stats,
                Err(e) => {
                    warn!(cache_key = %key.cache_key, error = %e, "archive extraction failed");
                    return Err(e.into());
                },
            };
        Ok(stats)
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

fn error_kind_for(error: &ExtractError) -> wit_types::ErrorKind {
    match error {
        ExtractError::UnsafePath(_)
        | ExtractError::PathTooDeep(_)
        | ExtractError::PathTooLong(_)
        | ExtractError::UnsupportedEntryKind(_)
        | ExtractError::Malformed(_) => wit_types::ErrorKind::InvalidInput,
        _ => wit_types::ErrorKind::Internal,
    }
}

/// Regression coverage for archive key stability and restart-safe materialization.
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
            cache_key: "pkg-1.0.crate".to_string(),
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

        assert_ne!(base.dir_name(), different_strip.dir_name());
        assert_ne!(base.dir_name(), different_format.dir_name());
        assert!(!base.dir_name().contains("pkg-1.0.crate"));
        assert!(base.dir_name().starts_with("targz-"));
    }

    fn insert_archive_blob(
        cache: &BlobCache,
        cache_key: &str,
        blob_path: PathBuf,
        archive_bytes: &[u8],
    ) -> u64 {
        std::fs::write(&blob_path, archive_bytes).unwrap();
        let record = cache.store(
            cache_key.to_string(),
            BlobMetadata {
                status: 200,
                content_type: Some("application/x-gzip".into()),
                etag: None,
                response_headers: Vec::new(),
                size: archive_bytes.len() as u64,
            },
        );
        record.id
    }

    #[tokio::test]
    async fn open_archive_returns_tree_ref_resolving_to_extracted_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let blob_cache_dir = tmp.path().join("blobs");
        let archive_root = tmp.path().join("archives");
        std::fs::create_dir_all(&blob_cache_dir).unwrap();
        std::fs::create_dir_all(&archive_root).unwrap();

        let cache = Arc::new(BlobCache::new(blob_cache_dir.clone()));

        let blob_path = blob_cache_dir.join("pkg-1.0.crate");
        let blob_id = insert_archive_blob(&cache, "pkg-1.0.crate", blob_path, &synthesize_targz());

        let trees = Arc::new(TreeRefs::new());
        let extractor = Arc::new(
            ArchiveExtractorComponent::new(archive_tool::DEFAULT_LIMITS).expect("build extractor"),
        );
        let executor = Arc::new(ArchiveExecutor::new(
            cache,
            trees.clone(),
            archive_root,
            extractor,
        ));

        let response = executor
            .open_archive(blob_id, ArchiveFormat::TarGz, Some("pkg-1.0/"))
            .await;
        let tree = match response {
            wit_types::CalloutResult::ArchiveOpened(wit_types::ArchiveOpened { tree: t }) => t,
            other => panic!("unexpected response: {other:?}"),
        };

        let extracted = trees.resolve(tree).expect("tree-ref resolves");
        let cargo = std::fs::read_to_string(extracted.join("Cargo.toml")).unwrap();
        assert!(cargo.contains("name = \"pkg\""));
    }

    #[tokio::test]
    async fn same_blob_with_distinct_strip_prefixes_keeps_stable_tree_refs() {
        let tmp = tempfile::tempdir().unwrap();
        let blob_cache_dir = tmp.path().join("blobs");
        let archive_root = tmp.path().join("archives");
        std::fs::create_dir_all(&blob_cache_dir).unwrap();
        std::fs::create_dir_all(&archive_root).unwrap();

        let cache = Arc::new(BlobCache::new(blob_cache_dir.clone()));
        let blob_id = insert_archive_blob(
            &cache,
            "multi-root.crate",
            blob_cache_dir.join("multi-root.crate"),
            &synthesize_multi_root_targz(),
        );

        let trees = Arc::new(TreeRefs::new());
        let extractor = Arc::new(
            ArchiveExtractorComponent::new(archive_tool::DEFAULT_LIMITS).expect("build extractor"),
        );
        let executor = Arc::new(ArchiveExecutor::new(
            cache,
            trees.clone(),
            archive_root,
            extractor,
        ));

        let alpha = match executor
            .open_archive(blob_id, ArchiveFormat::TarGz, Some("alpha/"))
            .await
        {
            wit_types::CalloutResult::ArchiveOpened(wit_types::ArchiveOpened { tree }) => tree,
            other => panic!("unexpected alpha response: {other:?}"),
        };
        let alpha_root = trees.resolve(alpha).expect("alpha tree-ref resolves");
        assert_eq!(
            std::fs::read_to_string(alpha_root.join("only.txt")).unwrap(),
            "alpha\n"
        );

        let beta = match executor
            .open_archive(blob_id, ArchiveFormat::TarGz, Some("beta/"))
            .await
        {
            wit_types::CalloutResult::ArchiveOpened(wit_types::ArchiveOpened { tree }) => tree,
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

    #[tokio::test]
    async fn resolved_archive_tree_ref_supports_nested_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        let blob_cache_dir = tmp.path().join("blobs");
        let archive_root = tmp.path().join("archives");
        std::fs::create_dir_all(&blob_cache_dir).unwrap();
        std::fs::create_dir_all(&archive_root).unwrap();

        let cache = Arc::new(BlobCache::new(blob_cache_dir.clone()));
        let blob_id = insert_archive_blob(
            &cache,
            "pkg-1.0.crate",
            blob_cache_dir.join("pkg-1.0.crate"),
            &synthesize_targz(),
        );

        let trees = Arc::new(TreeRefs::new());
        let extractor = Arc::new(
            ArchiveExtractorComponent::new(archive_tool::DEFAULT_LIMITS).expect("build extractor"),
        );
        let executor = Arc::new(ArchiveExecutor::new(
            cache,
            trees.clone(),
            archive_root,
            extractor,
        ));

        let tree = match executor
            .open_archive(blob_id, ArchiveFormat::TarGz, Some("pkg-1.0/"))
            .await
        {
            wit_types::CalloutResult::ArchiveOpened(wit_types::ArchiveOpened { tree }) => tree,
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
        let key = ExtractKey {
            cache_key: "pkg-1.0.crate".into(),
            format: ArchiveFormat::TarGz,
            strip_prefix: Some("pkg-1.0/".into()),
        };
        let keep = archive_root.join(key.dir_name());
        std::fs::create_dir_all(&keep).unwrap();

        let cache = Arc::new(BlobCache::new(blob_cache_dir));
        let trees = Arc::new(TreeRefs::new());
        let extractor = Arc::new(
            ArchiveExtractorComponent::new(archive_tool::DEFAULT_LIMITS).expect("build extractor"),
        );
        let _executor = ArchiveExecutor::new(cache, trees, archive_root, extractor);

        assert!(!stale.exists());
        assert!(keep.exists());
    }

    #[tokio::test]
    async fn fresh_materializer_reuses_existing_extracted_tree_after_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let blob_cache_dir = tmp.path().join("blobs");
        let archive_root = tmp.path().join("archives");
        std::fs::create_dir_all(&blob_cache_dir).unwrap();
        std::fs::create_dir_all(&archive_root).unwrap();

        let cache = Arc::new(BlobCache::new(blob_cache_dir.clone()));
        let blob_id = insert_archive_blob(
            &cache,
            "pkg-1.0.crate",
            blob_cache_dir.join("pkg-1.0.crate"),
            &synthesize_targz(),
        );

        let trees = Arc::new(TreeRefs::new());
        let extractor = Arc::new(
            ArchiveExtractorComponent::new(archive_tool::DEFAULT_LIMITS).expect("build extractor"),
        );
        let executor = Arc::new(ArchiveExecutor::new(
            cache.clone(),
            trees.clone(),
            archive_root.clone(),
            extractor.clone(),
        ));

        let tree = match executor
            .open_archive(blob_id, ArchiveFormat::TarGz, Some("pkg-1.0/"))
            .await
        {
            wit_types::CalloutResult::ArchiveOpened(wit_types::ArchiveOpened { tree }) => tree,
            other => panic!("unexpected response: {other:?}"),
        };
        let first_root = trees.resolve(tree).expect("tree-ref resolves");
        let marker = first_root.join(".reuse-marker");
        std::fs::write(&marker, b"stable").unwrap();

        let second_trees = Arc::new(TreeRefs::new());
        let second_executor = Arc::new(ArchiveExecutor::new(
            cache,
            second_trees.clone(),
            archive_root.clone(),
            extractor,
        ));

        let tree = match second_executor
            .open_archive(blob_id, ArchiveFormat::TarGz, Some("pkg-1.0/"))
            .await
        {
            wit_types::CalloutResult::ArchiveOpened(wit_types::ArchiveOpened { tree }) => tree,
            other => panic!("unexpected response: {other:?}"),
        };
        let second_root = second_trees.resolve(tree).expect("tree-ref resolves");
        assert_eq!(first_root, second_root);
        assert!(marker.exists());
    }
}
