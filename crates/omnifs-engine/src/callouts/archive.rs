//! Mount a stored blob as a directory tree by extracting it on the host.
//!
//! The provider issues `open-archive(blob, format, strip-prefix)`; the
//! host extracts the blob into a fresh directory via
//! [`crate::tools::archive`] and, on success, registers the resulting
//! directory in the shared [`TreeRefs`]. The provider returns the
//! resulting `tree-ref`, which the host serves through the same FUSE
//! bind-mount path that already serves git clones. There are no
//! per-file callouts; extraction runs once, directly on the host.

#[cfg(test)]
use crate::blob_cache::{BLOB_TMP_DIR, BlobMetadata};
use crate::blob_cache::{BlobCache, BlobRecord};
use crate::cache::identity::BlobGeneration;
#[cfg(test)]
use crate::cache::identity::BlobRequestId;
use crate::callouts::{callout_error, callout_internal, callout_not_found, record_outcome};
use crate::sandbox::publish;
use crate::tools::archive::{self, ArchiveFormat, ExtractError, ExtractStats};
use crate::tree_refs::TreeRefs;
use dashmap::DashMap;
use omnifs_wit::provider::types as wit_types;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{debug, warn};

/// Per-provider archive extractor. Owns the on-disk extraction root and
/// shares a [`TreeRefs`] with the git executor so a returned
/// `tree-ref` resolves identically regardless of source. Extraction
/// itself is performed on the host by [`crate::tools::archive::extract`].
///
/// Cached extraction directories are keyed by the full `(blob generation,
/// format, strip-prefix)` view and published with a temporary directory
/// rename, so a registered `tree-ref` never points at a partial tree.
pub(crate) struct ArchiveExecutor {
    cache: Arc<BlobCache>,
    extract_root: PathBuf,
    trees: Arc<TreeRefs>,
    trees_by_key: DashMap<ExtractKey, u64>,
    locks: DashMap<ExtractKey, Arc<Mutex<()>>>,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct ExtractKey {
    generation: BlobGeneration,
    format: ArchiveFormat,
    strip_prefix: Option<String>,
}

impl ExtractKey {
    fn dir_name(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.generation.filesystem_name().as_bytes());
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
            hex::encode(&digest[..16])
        )
    }
}

#[derive(Debug)]
enum ArchiveMaterialized {
    Cached { tree: u64 },
    Fresh { tree: u64, stats: ExtractStats },
}

#[derive(Debug, thiserror::Error)]
enum ArchiveError {
    #[error("{0}")]
    Extract(#[from] ExtractError),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Internal(String),
}

impl From<ArchiveError> for wit_types::CalloutResult {
    fn from(error: ArchiveError) -> Self {
        match error {
            ArchiveError::Extract(e) => {
                callout_error(wit_types::ErrorKind::from(&e), e.to_string(), false)
            },
            ArchiveError::NotFound(msg) => callout_not_found(msg),
            ArchiveError::Internal(msg) => callout_internal(msg),
        }
    }
}

impl ArchiveExecutor {
    pub(crate) fn new(
        cache: Arc<BlobCache>,
        trees: Arc<TreeRefs>,
        extract_root: PathBuf,
    ) -> std::io::Result<Self> {
        publish::sweep_temp_publish_dirs(&extract_root)?;
        Ok(Self {
            cache,
            extract_root,
            trees,
            trees_by_key: DashMap::new(),
            locks: DashMap::new(),
        })
    }

    #[tracing::instrument(target = "omnifs_callout", skip_all, fields(
        blob = req.blob,
        format = ?req.format,
        strip_prefix = req.strip_prefix.as_deref().unwrap_or(""),
        tree_ref = tracing::field::Empty,
        error.kind = tracing::field::Empty,
        error.message = tracing::field::Empty,
        error.retryable = tracing::field::Empty,
    ))]
    pub(crate) async fn open(
        self: &Arc<Self>,
        req: &wit_types::ArchiveOpenRequest,
    ) -> wit_types::CalloutResult {
        let this = Arc::clone(self);
        let blob_id = req.blob;
        let format = ArchiveFormat::from(req.format);
        let strip = req.strip_prefix.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            this.open_archive_blocking(blob_id, format, strip.as_deref())
        })
        .await;
        let result = match outcome {
            Ok(Ok(tree)) => {
                wit_types::CalloutResult::ArchiveOpened(wit_types::ArchiveOpened { tree })
            },
            Ok(Err(e)) => e.into(),
            Err(join_err) => {
                ArchiveError::Internal(format!("extract task join: {join_err}")).into()
            },
        };
        record_outcome(&result);
        result
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
        let key = ExtractKey {
            generation: record.generation,
            format,
            strip_prefix,
        };

        let tree = match self.materialize(&key, &record)? {
            ArchiveMaterialized::Cached { tree } => tree,
            ArchiveMaterialized::Fresh { tree, stats } => {
                debug!(
                    generation = %key.generation,
                    entries = stats.entries,
                    bytes = stats.bytes_written,
                    "archive extracted"
                );
                tree
            },
        };
        Ok(tree)
    }

    fn materialize(
        &self,
        key: &ExtractKey,
        record: &BlobRecord,
    ) -> Result<ArchiveMaterialized, ArchiveError> {
        if let Some(tree) = self.trees_by_key.get(key).map(|entry| *entry) {
            return Ok(ArchiveMaterialized::Cached { tree });
        }

        let lock = self
            .locks
            .entry(key.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _guard = lock.lock();

        if let Some(tree) = self.trees_by_key.get(key).map(|entry| *entry) {
            self.locks.remove(key);
            return Ok(ArchiveMaterialized::Cached { tree });
        }

        if let Err(error) = std::fs::create_dir_all(&self.extract_root) {
            self.locks.remove(key);
            return Err(ArchiveError::Internal(format!(
                "prepare archive extraction: {error}"
            )));
        }
        let dest = self.extract_root.join(key.dir_name());

        if let Ok(metadata) = std::fs::symlink_metadata(&dest) {
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                let tree = self.trees.register(dest);
                self.trees_by_key.insert(key.clone(), tree);
                self.locks.remove(key);
                return Ok(ArchiveMaterialized::Cached { tree });
            }
            if let Err(error) = publish::remove_existing_path(&dest) {
                self.locks.remove(key);
                return Err(ArchiveError::Internal(format!(
                    "prepare archive extraction: {error}"
                )));
            }
        }

        let tmp = publish::temp_sibling_path(&dest);
        if tmp.exists()
            && let Err(error) = publish::remove_existing_path(&tmp)
        {
            self.locks.remove(key);
            return Err(ArchiveError::Internal(format!(
                "prepare archive extraction: {error}"
            )));
        }
        if let Err(error) = std::fs::create_dir_all(&tmp) {
            self.locks.remove(key);
            return Err(ArchiveError::Internal(format!(
                "prepare archive extraction: {error}"
            )));
        }

        let stats = match self.extract_to(&tmp, key, record) {
            Ok(stats) => stats,
            Err(error) => {
                publish::remove_path_best_effort(&tmp);
                self.locks.remove(key);
                return Err(error);
            },
        };

        if let Err(error) = std::fs::rename(&tmp, &dest) {
            publish::remove_path_best_effort(&tmp);
            self.locks.remove(key);
            return Err(ArchiveError::Internal(format!(
                "publish archive extraction: {error}"
            )));
        }

        let tree = self.trees.register(dest);
        self.trees_by_key.insert(key.clone(), tree);
        self.locks.remove(key);
        Ok(ArchiveMaterialized::Fresh { tree, stats })
    }

    fn extract_to(
        &self,
        tmp: &Path,
        key: &ExtractKey,
        record: &BlobRecord,
    ) -> Result<ExtractStats, ArchiveError> {
        let blob_path = self.cache.generation_path(record.generation);
        let stats = match archive::extract(
            key.format,
            &blob_path,
            tmp,
            key.strip_prefix.as_deref(),
            archive::DEFAULT_LIMITS,
        ) {
            Ok(stats) => stats,
            Err(e) => {
                warn!(generation = %key.generation, error = %e, "archive extraction failed");
                return Err(e.into());
            },
        };
        Ok(stats)
    }
}

impl From<wit_types::ArchiveFormat> for ArchiveFormat {
    fn from(format: wit_types::ArchiveFormat) -> Self {
        match format {
            wit_types::ArchiveFormat::TarGz => Self::TarGz,
            wit_types::ArchiveFormat::Tar => Self::Tar,
            wit_types::ArchiveFormat::Zip => Self::Zip,
        }
    }
}

impl From<&ExtractError> for wit_types::ErrorKind {
    fn from(error: &ExtractError) -> Self {
        match error {
            ExtractError::UnsafePath(_)
            | ExtractError::PathTooDeep(_)
            | ExtractError::PathTooLong(_)
            | ExtractError::UnsupportedEntryKind(_)
            | ExtractError::Malformed(_) => Self::InvalidInput,
            _ => Self::Internal,
        }
    }
}

/// Regression coverage for archive key stability and restart-safe materialization.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::archive::tests::append_targz_file;
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

    #[test]
    fn extraction_dir_name_changes_with_full_key() {
        let base = ExtractKey {
            generation: BlobGeneration::from_bytes(b"pkg-1.0.crate"),
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

    fn open_request_targz(blob: u64, strip_prefix: &str) -> wit_types::ArchiveOpenRequest {
        wit_types::ArchiveOpenRequest {
            blob,
            format: wit_types::ArchiveFormat::TarGz,
            strip_prefix: Some(strip_prefix.to_string()),
        }
    }

    fn insert_archive_blob(cache: &BlobCache, blob_path: &Path, archive_bytes: &[u8]) -> u64 {
        let generation = BlobGeneration::from_bytes(archive_bytes);
        let request = BlobRequestId::new(
            None,
            "GET",
            &format!("https://archive.example.test/{}", blob_path.display()),
            &[],
            None,
        );
        let staged = cache.cache_dir().join(BLOB_TMP_DIR).join("archive-stage");
        std::fs::write(&staged, archive_bytes).unwrap();
        let record = cache
            .publish(
                request,
                generation,
                &staged,
                BlobMetadata {
                    status: 200,
                    content_type: Some("application/x-gzip".into()),
                    etag: None,
                    response_headers: Vec::new(),
                    size: archive_bytes.len() as u64,
                },
            )
            .unwrap();
        record.id
    }

    #[tokio::test]
    async fn open_archive_returns_tree_ref_resolving_to_extracted_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let blob_cache_dir = tmp.path().join("blobs");
        let archive_root = tmp.path().join("archives");
        std::fs::create_dir_all(&blob_cache_dir).unwrap();
        std::fs::create_dir_all(&archive_root).unwrap();

        let cache = Arc::new(BlobCache::new(blob_cache_dir.clone()).unwrap());

        let blob_path = blob_cache_dir.join("pkg-1.0.crate");
        let blob_id = insert_archive_blob(&cache, &blob_path, &synthesize_targz());

        let trees = Arc::new(TreeRefs::new());
        let executor = Arc::new(ArchiveExecutor::new(cache, trees.clone(), archive_root).unwrap());

        let response = executor
            .open(&open_request_targz(blob_id, "pkg-1.0/"))
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

        let cache = Arc::new(BlobCache::new(blob_cache_dir.clone()).unwrap());
        let blob_id = insert_archive_blob(
            &cache,
            &blob_cache_dir.join("multi-root.crate"),
            &synthesize_multi_root_targz(),
        );

        let trees = Arc::new(TreeRefs::new());
        let executor = Arc::new(ArchiveExecutor::new(cache, trees.clone(), archive_root).unwrap());

        let alpha = match executor.open(&open_request_targz(blob_id, "alpha/")).await {
            wit_types::CalloutResult::ArchiveOpened(wit_types::ArchiveOpened { tree }) => tree,
            other => panic!("unexpected alpha response: {other:?}"),
        };
        let alpha_root = trees.resolve(alpha).expect("alpha tree-ref resolves");
        assert_eq!(
            std::fs::read_to_string(alpha_root.join("only.txt")).unwrap(),
            "alpha\n"
        );

        let beta = match executor.open(&open_request_targz(blob_id, "beta/")).await {
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

        let cache = Arc::new(BlobCache::new(blob_cache_dir.clone()).unwrap());
        let blob_id = insert_archive_blob(
            &cache,
            &blob_cache_dir.join("pkg-1.0.crate"),
            &synthesize_targz(),
        );

        let trees = Arc::new(TreeRefs::new());
        let executor = Arc::new(ArchiveExecutor::new(cache, trees.clone(), archive_root).unwrap());

        let tree = match executor
            .open(&open_request_targz(blob_id, "pkg-1.0/"))
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
            generation: BlobGeneration::from_bytes(b"pkg-1.0.crate"),
            format: ArchiveFormat::TarGz,
            strip_prefix: Some("pkg-1.0/".into()),
        };
        let keep = archive_root.join(key.dir_name());
        std::fs::create_dir_all(&keep).unwrap();

        let cache = Arc::new(BlobCache::new(blob_cache_dir).unwrap());
        let trees = Arc::new(TreeRefs::new());
        let _executor = ArchiveExecutor::new(cache, trees, archive_root).unwrap();

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

        let cache = Arc::new(BlobCache::new(blob_cache_dir.clone()).unwrap());
        let blob_id = insert_archive_blob(
            &cache,
            &blob_cache_dir.join("pkg-1.0.crate"),
            &synthesize_targz(),
        );

        let trees = Arc::new(TreeRefs::new());
        let executor = Arc::new(
            ArchiveExecutor::new(cache.clone(), trees.clone(), archive_root.clone()).unwrap(),
        );

        let tree = match executor
            .open(&open_request_targz(blob_id, "pkg-1.0/"))
            .await
        {
            wit_types::CalloutResult::ArchiveOpened(wit_types::ArchiveOpened { tree }) => tree,
            other => panic!("unexpected response: {other:?}"),
        };
        let first_root = trees.resolve(tree).expect("tree-ref resolves");
        let marker = first_root.join(".reuse-marker");
        std::fs::write(&marker, b"stable").unwrap();

        let second_trees = Arc::new(TreeRefs::new());
        let second_executor = Arc::new(
            ArchiveExecutor::new(cache, second_trees.clone(), archive_root.clone()).unwrap(),
        );

        let tree = match second_executor
            .open(&open_request_targz(blob_id, "pkg-1.0/"))
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
