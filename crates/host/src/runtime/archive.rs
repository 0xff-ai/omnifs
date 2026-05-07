//! Mount a stored blob as a directory tree by extracting it to disk.
//!
//! The provider issues `open-archive(blob, format, strip-prefix)`; the
//! host walks the blob in place, materializes a real directory under
//! `<cache_dir>/archives/<provider-scope>/<blob-id>/`, and returns a
//! `tree-ref` that resolves through the same registry git uses. Once
//! extracted, listings and reads are served by the FUSE bind-mount path
//! that already serves git clones — no per-file callouts.

use crate::runtime::blob::{BlobCache, BlobRecord};
use crate::runtime::executor::{CalloutResponse, ErrorKind};
use crate::runtime::tree_registry::{TreeRegistry, is_safe_relative_path};
use dashmap::DashMap;
use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveFormat {
    TarGz,
    Tar,
    Zip,
}

#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error("blob {0} not found")]
    BlobNotFound(u64),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("zip error: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("entry path {0} escapes archive root")]
    UnsafePath(String),
    #[error("internal: {0}")]
    Internal(String),
}

/// Per-provider archive extractor. Owns the on-disk extraction root and
/// shares a [`TreeRegistry`] with the git executor so a returned
/// `tree-ref` resolves identically regardless of source.
pub struct ArchiveExecutor {
    cache: Arc<BlobCache>,
    trees: Arc<TreeRegistry>,
    extract_root: PathBuf,
    /// `(blob-id, format-tag, strip-prefix)` → cached `tree-ref`. An
    /// archive is extracted at most once per `(blob, params)` triple
    /// even across concurrent open-archive calls.
    extracted: DashMap<ExtractKey, u64>,
    /// Per-key lock to coalesce concurrent extractions.
    locks: DashMap<ExtractKey, Arc<Mutex<()>>>,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct ExtractKey {
    blob_id: u64,
    format: u8,
    strip_prefix: String,
}

impl ArchiveExecutor {
    pub fn new(cache: Arc<BlobCache>, trees: Arc<TreeRegistry>, extract_root: PathBuf) -> Self {
        Self {
            cache,
            trees,
            extract_root,
            extracted: DashMap::new(),
            locks: DashMap::new(),
        }
    }

    pub fn open_archive(
        &self,
        blob_id: u64,
        format: ArchiveFormat,
        strip_prefix: Option<&str>,
    ) -> CalloutResponse {
        let strip_prefix = strip_prefix.unwrap_or("").to_string();
        let key = ExtractKey {
            blob_id,
            format: format_tag(format),
            strip_prefix: strip_prefix.clone(),
        };

        // Coalesce concurrent extractions of the same (blob, params).
        let lock = self
            .locks
            .entry(key.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _guard = lock.lock();

        if let Some(id) = self.extracted.get(&key).map(|r| *r)
            && self.trees.contains(id)
        {
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

        // Re-extract every time on first request, but if we get a cache
        // hit on `extracted` above, the existing dir is reused. A torn
        // extraction (host crash mid-extract) is recovered by removing
        // the dir on this codepath; new extraction proceeds.
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

        let strip = if strip_prefix.is_empty() {
            None
        } else {
            Some(strip_prefix.as_str())
        };
        if let Err(e) = extract(format, &record, &dest, strip) {
            let _ = std::fs::remove_dir_all(&dest);
            tracing::warn!(blob = blob_id, error = %e, "archive extraction failed");
            return error(error_kind_for(&e), e.to_string(), false);
        }

        let id = self.trees.register(dest);
        self.extracted.insert(key, id);
        CalloutResponse::ArchiveOpened(id)
    }
}

fn format_tag(format: ArchiveFormat) -> u8 {
    match format {
        ArchiveFormat::TarGz => 1,
        ArchiveFormat::Tar => 2,
        ArchiveFormat::Zip => 3,
    }
}

fn error_kind_for(error: &ArchiveError) -> ErrorKind {
    match error {
        ArchiveError::BlobNotFound(_) => ErrorKind::NotFound,
        ArchiveError::UnsafePath(_) => ErrorKind::InvalidInput,
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

fn extract(
    format: ArchiveFormat,
    record: &BlobRecord,
    dest: &Path,
    strip_prefix: Option<&str>,
) -> Result<(), ArchiveError> {
    match format {
        ArchiveFormat::TarGz => extract_tar_gz(&record.path, dest, strip_prefix),
        ArchiveFormat::Tar => extract_tar(&record.path, dest, strip_prefix),
        ArchiveFormat::Zip => extract_zip(&record.path, dest, strip_prefix),
    }
}

fn extract_tar_gz(
    blob_path: &Path,
    dest: &Path,
    strip_prefix: Option<&str>,
) -> Result<(), ArchiveError> {
    let file = std::fs::File::open(blob_path)?;
    let decoder = flate2::read::GzDecoder::new(file);
    extract_tar_reader(decoder, dest, strip_prefix)
}

fn extract_tar(
    blob_path: &Path,
    dest: &Path,
    strip_prefix: Option<&str>,
) -> Result<(), ArchiveError> {
    let file = std::fs::File::open(blob_path)?;
    extract_tar_reader(file, dest, strip_prefix)
}

fn extract_tar_reader<R: std::io::Read>(
    reader: R,
    dest: &Path,
    strip_prefix: Option<&str>,
) -> Result<(), ArchiveError> {
    let mut archive = tar::Archive::new(reader);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry
            .path()
            .map_err(|e| ArchiveError::Internal(e.to_string()))?
            .into_owned();

        let Some(target_rel) = strip_prefix_path(&path, strip_prefix) else {
            continue;
        };
        if !is_safe_relative_path(&target_rel) {
            return Err(ArchiveError::UnsafePath(target_rel.display().to_string()));
        }

        let target = dest.join(&target_rel);
        let header = entry.header();
        let kind = header.entry_type();
        if kind.is_dir() {
            std::fs::create_dir_all(&target)?;
            continue;
        }
        if kind.is_symlink() || kind.is_hard_link() {
            // Skip links to avoid escape-into-host-fs surprises in v1.
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = std::fs::File::create(&target)?;
        std::io::copy(&mut entry, &mut out)?;
    }
    Ok(())
}

fn extract_zip(
    blob_path: &Path,
    dest: &Path,
    strip_prefix: Option<&str>,
) -> Result<(), ArchiveError> {
    let file = std::fs::File::open(blob_path)?;
    let mut archive = zip::ZipArchive::new(file)?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let Some(enclosed) = entry.enclosed_name() else {
            return Err(ArchiveError::UnsafePath(entry.name().to_string()));
        };
        let Some(target_rel) = strip_prefix_path(&enclosed, strip_prefix) else {
            continue;
        };
        if !is_safe_relative_path(&target_rel) {
            return Err(ArchiveError::UnsafePath(target_rel.display().to_string()));
        }
        let target = dest.join(&target_rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&target)?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = std::fs::File::create(&target)?;
        std::io::copy(&mut entry, &mut out)?;
    }
    Ok(())
}

/// Strip `prefix` from the front of `path`. Returns `None` when the
/// prefix is non-empty and `path` doesn't start with it (entry skipped).
/// When `path == prefix` after normalization, returns an empty path
/// (the archive root), which is also treated as a skip.
fn strip_prefix_path(path: &Path, prefix: Option<&str>) -> Option<PathBuf> {
    let Some(prefix) = prefix else {
        return Some(path.to_path_buf());
    };
    if prefix.is_empty() {
        return Some(path.to_path_buf());
    }
    let prefix_path = Path::new(prefix.trim_end_matches('/'));
    let stripped = path.strip_prefix(prefix_path).ok()?;
    if stripped.as_os_str().is_empty() {
        None
    } else {
        Some(stripped.to_path_buf())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::blob::BlobRecord;
    use std::io::Write;

    #[test]
    fn strip_prefix_keeps_unstripped_when_none() {
        let p = Path::new("foo/bar.txt");
        assert_eq!(strip_prefix_path(p, None), Some(p.to_path_buf()));
        assert_eq!(strip_prefix_path(p, Some("")), Some(p.to_path_buf()));
    }

    #[test]
    fn strip_prefix_strips_leading_directory() {
        let p = Path::new("serde-1.0.197/Cargo.toml");
        assert_eq!(
            strip_prefix_path(p, Some("serde-1.0.197/")),
            Some(PathBuf::from("Cargo.toml"))
        );
        assert_eq!(
            strip_prefix_path(p, Some("serde-1.0.197")),
            Some(PathBuf::from("Cargo.toml"))
        );
    }

    #[test]
    fn strip_prefix_skips_root_and_mismatch() {
        assert_eq!(
            strip_prefix_path(Path::new("serde-1.0.197"), Some("serde-1.0.197/")),
            None
        );
        assert_eq!(
            strip_prefix_path(Path::new("other/Cargo.toml"), Some("serde-1.0.197/")),
            None
        );
    }

    /// Build an in-memory `.tar.gz` containing two files under
    /// `pkg-1.0/`. Used by extract tests to verify gzip + tar walk
    /// without going through HTTP.
    fn synthesize_targz() -> Vec<u8> {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        {
            let mut tar = tar::Builder::new(&mut gz);
            let cargo_toml = b"[package]\nname = \"pkg\"\nversion = \"1.0.0\"\n";
            let mut header = tar::Header::new_gnu();
            header.set_path("pkg-1.0/Cargo.toml").unwrap();
            header.set_size(cargo_toml.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append(&header, &cargo_toml[..]).unwrap();

            let lib_rs = b"pub fn answer() -> u32 { 42 }\n";
            let mut header = tar::Header::new_gnu();
            header.set_path("pkg-1.0/src/lib.rs").unwrap();
            header.set_size(lib_rs.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append(&header, &lib_rs[..]).unwrap();

            tar.finish().unwrap();
        }
        gz.finish().unwrap()
    }

    fn write_blob(dir: &std::path::Path, name: &str, bytes: &[u8]) -> BlobRecord {
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(bytes).unwrap();
        BlobRecord {
            id: 1,
            path,
            size: bytes.len() as u64,
            content_type: Some("application/x-gzip".into()),
            etag: None,
            status: 200,
            response_headers: Vec::new(),
        }
    }

    #[test]
    fn extract_tar_gz_writes_entries_and_strips_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let blob_dir = tmp.path().join("blobs");
        std::fs::create_dir_all(&blob_dir).unwrap();
        let dest = tmp.path().join("extracted");
        std::fs::create_dir_all(&dest).unwrap();

        let archive_bytes = synthesize_targz();
        let record = write_blob(&blob_dir, "pkg-1.0.crate", &archive_bytes);
        extract(ArchiveFormat::TarGz, &record, &dest, Some("pkg-1.0/")).unwrap();

        let cargo = std::fs::read_to_string(dest.join("Cargo.toml")).unwrap();
        assert!(cargo.contains("name = \"pkg\""));
        let lib = std::fs::read_to_string(dest.join("src").join("lib.rs")).unwrap();
        assert!(lib.contains("answer"));
    }

    #[test]
    fn open_archive_returns_tree_ref_resolving_to_extracted_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let blob_cache_dir = tmp.path().join("blobs");
        let archive_root = tmp.path().join("archives");
        std::fs::create_dir_all(&blob_cache_dir).unwrap();
        std::fs::create_dir_all(&archive_root).unwrap();

        let cache = Arc::new(BlobCache::new(blob_cache_dir.clone()));

        // Hand-craft a blob entry without going through HTTP.
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
        let executor = ArchiveExecutor::new(cache, trees.clone(), archive_root);

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
