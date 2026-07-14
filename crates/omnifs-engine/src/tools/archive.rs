//! Host-native archive extraction.
//!
//! Walks a stored archive blob (`tar.gz`, `tar`, or `zip`) and writes
//! its sanitized entries under a destination directory. Runs directly in
//! the host: the source archive's libraries (`tar`, `zip`, `flate2`) are
//! memory-safe Rust, so extraction needs no WASM sandbox. Two backstops
//! stand in for the guarantees the sandbox used to provide:
//!
//! - **Path containment.** [`sanitize_path`] rejects absolute paths,
//!   `..`, and over-deep/over-long names; symlinks and hard links are
//!   refused outright. Because no symlinks are ever created and every
//!   parent directory is created by us, lexical containment under the
//!   destination root is sound.
//! - **Resource caps.** Entry count, per-file size, total bytes, and a
//!   wall-clock deadline are enforced as the walk proceeds, bounding both
//!   output size and CPU for the single decompression layer we perform.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Defaults the host applies to every extraction. Tuned to comfortably
/// cover crates.io-class tarballs (median <1 MiB, largest a few hundred
/// MiB) while still rejecting zip-bombs.
pub const DEFAULT_LIMITS: ExtractorLimits = ExtractorLimits {
    max_entries: 50_000,
    max_file_size: 256 * 1024 * 1024,
    max_total_bytes: 1024 * 1024 * 1024,
    max_path_depth: 64,
    max_path_len: 4096,
    max_duration: Duration::from_mins(5),
};

/// Resource and shape limits enforced for one archive extraction.
#[derive(Clone, Copy, Debug)]
#[allow(clippy::struct_field_names)]
pub struct ExtractorLimits {
    /// Maximum number of archive entries that may be written.
    pub max_entries: u64,
    /// Maximum size of any single extracted file.
    pub max_file_size: u64,
    /// Maximum total bytes written across all extracted files.
    pub max_total_bytes: u64,
    /// Maximum path component depth after optional prefix stripping.
    pub max_path_depth: u32,
    /// Maximum rendered path length after optional prefix stripping.
    pub max_path_len: u32,
    /// Wall-clock budget for the whole extraction; replaces the WASM
    /// fuel cap as the bound on decompression CPU.
    pub max_duration: Duration,
}

/// Archive formats the host knows how to extract.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ArchiveFormat {
    /// Gzip-compressed tar archive.
    TarGz,
    /// Uncompressed tar archive.
    Tar,
    /// Zip archive.
    Zip,
}

impl ArchiveFormat {
    pub(crate) const fn cache_component(self) -> &'static str {
        match self {
            Self::TarGz => "targz",
            Self::Tar => "tar",
            Self::Zip => "zip",
        }
    }
}

/// Failure returned by the host extractor.
#[derive(Debug, thiserror::Error)]
pub enum ExtractError {
    #[error("too many entries")]
    TooManyEntries,
    #[error("file too large: {0}")]
    FileTooLarge(String),
    #[error("total bytes exceeded")]
    TotalTooLarge,
    #[error("path too deep: {0}")]
    PathTooDeep(String),
    #[error("path too long: {0}")]
    PathTooLong(String),
    #[error("unsafe path: {0}")]
    UnsafePath(String),
    #[error("unsupported entry kind: {0}")]
    UnsupportedEntryKind(String),
    #[error("malformed archive: {0}")]
    Malformed(String),
    #[error("extraction timed out")]
    TimedOut,
    #[error("io: {0}")]
    Io(String),
}

/// Extraction counters reported back to the caller.
#[derive(Debug, Clone, Copy, Default)]
pub struct ExtractStats {
    /// Number of archive entries written to the destination tree.
    pub entries: u64,
    /// Total file bytes written to the destination tree.
    pub bytes_written: u64,
}

/// Extract `blob_path` (a regular file in the requested `format`) into
/// `dest_dir` (which the caller must have created), enforcing `limits`.
/// `strip_prefix`, when set, drops a leading directory from each entry's
/// path before it lands on disk.
pub fn extract(
    format: ArchiveFormat,
    blob_path: &Path,
    dest_dir: &Path,
    strip_prefix: Option<&str>,
    limits: ExtractorLimits,
) -> Result<ExtractStats, ExtractError> {
    let blob = std::fs::File::open(blob_path)
        .map_err(|e| ExtractError::Io(format!("open {}: {e}", blob_path.display())))?;
    let deadline = Instant::now() + limits.max_duration;
    let mut extraction = Extraction::new(dest_dir, strip_prefix, limits, deadline);
    match format {
        ArchiveFormat::TarGz => {
            extraction.run_tar(flate2::read::GzDecoder::new(blob))?;
        },
        ArchiveFormat::Tar => {
            extraction.run_tar(blob)?;
        },
        ArchiveFormat::Zip => {
            extraction.run_zip(blob)?;
        },
    }
    Ok(extraction.counter.into_stats())
}

#[derive(Default)]
struct Counter {
    entries: u64,
    bytes_written: u64,
}

impl Counter {
    fn check_entry_count(&self, limits: &ExtractorLimits) -> Result<(), ExtractError> {
        (self.entries < limits.max_entries)
            .then_some(())
            .ok_or(ExtractError::TooManyEntries)
    }

    fn add_bytes(&mut self, n: u64, limits: &ExtractorLimits) -> Result<(), ExtractError> {
        let next = self
            .bytes_written
            .checked_add(n)
            .ok_or(ExtractError::TotalTooLarge)?;
        (next <= limits.max_total_bytes)
            .then_some(())
            .ok_or(ExtractError::TotalTooLarge)?;
        self.bytes_written = next;
        Ok(())
    }

    fn into_stats(self) -> ExtractStats {
        ExtractStats {
            entries: self.entries,
            bytes_written: self.bytes_written,
        }
    }
}

struct Extraction<'a> {
    dest_root: &'a Path,
    limits: ExtractorLimits,
    deadline: Instant,
    counter: Counter,
    buf: Vec<u8>,
    created_dirs: HashSet<PathBuf>,
    strip_prefix: Option<&'a str>,
}

impl<'a> Extraction<'a> {
    fn new(
        dest_root: &'a Path,
        strip_prefix: Option<&'a str>,
        limits: ExtractorLimits,
        deadline: Instant,
    ) -> Self {
        Self {
            dest_root,
            limits,
            deadline,
            counter: Counter::default(),
            buf: vec![0u8; 64 * 1024],
            created_dirs: HashSet::new(),
            strip_prefix,
        }
    }

    fn run_tar<R: Read>(&mut self, reader: R) -> Result<(), ExtractError> {
        let mut archive = tar::Archive::new(reader);
        let entries = archive
            .entries()
            .map_err(|e| ExtractError::Malformed(e.to_string()))?;
        for entry in entries {
            self.check_deadline()?;
            let mut entry = entry.map_err(|e| ExtractError::Malformed(e.to_string()))?;
            let raw_path = entry
                .path()
                .map_err(|e| ExtractError::Malformed(e.to_string()))?
                .into_owned();
            let Some(rel) = sanitize_path(&raw_path, self.strip_prefix, &self.limits)? else {
                continue;
            };
            let header = entry.header();
            let kind = header.entry_type();
            let shape = match (header, kind) {
                (_, kind) if kind.is_dir() => EntryShape::Dir,
                (_, kind) if kind.is_symlink() || kind.is_hard_link() => EntryShape::Link,
                (header, kind) if kind.is_file() => EntryShape::File {
                    size: header
                        .size()
                        .map_err(|e| ExtractError::Malformed(e.to_string()))?,
                },
                (_, kind) => EntryShape::Other(format!("{kind:?}")),
            };
            self.process_entry(&rel, shape, &mut entry)?;
        }
        Ok(())
    }

    fn run_zip<R: Read + std::io::Seek>(&mut self, reader: R) -> Result<(), ExtractError> {
        let mut archive =
            zip::ZipArchive::new(reader).map_err(|e| ExtractError::Malformed(e.to_string()))?;
        for i in 0..archive.len() {
            self.check_deadline()?;
            let mut entry = archive
                .by_index(i)
                .map_err(|e| ExtractError::Malformed(e.to_string()))?;
            let Some(enclosed) = entry.enclosed_name() else {
                return Err(ExtractError::UnsafePath(entry.name().to_string()));
            };
            let Some(rel) = sanitize_path(&enclosed, self.strip_prefix, &self.limits)? else {
                continue;
            };
            let shape = if entry.is_dir() {
                EntryShape::Dir
            } else if entry.is_symlink() {
                EntryShape::Link
            } else {
                EntryShape::File { size: entry.size() }
            };
            self.process_entry(&rel, shape, &mut entry)?;
        }
        Ok(())
    }

    /// Apply the post-classification work that's identical across every
    /// archive backend: count + cap-check, write or refuse, advance the
    /// total-bytes counter. Backends only have to convert their own
    /// per-entry shape into [`EntryShape`].
    fn process_entry<R: Read>(
        &mut self,
        rel: &Path,
        shape: EntryShape,
        reader: &mut R,
    ) -> Result<(), ExtractError> {
        match shape {
            EntryShape::Dir => {
                self.counter.check_entry_count(&self.limits)?;
                self.counter.entries += 1;
                let dest = self.dest_root.join(rel);
                self.ensure_dir(&dest)
            },
            EntryShape::Link => {
                // Refuse links: a benign archive can express its layout
                // without them, and resolving them is an attack class we
                // don't need to support.
                Err(ExtractError::UnsupportedEntryKind(format!(
                    "{} (link)",
                    rel.display()
                )))
            },
            EntryShape::Other(detail) => Err(ExtractError::UnsupportedEntryKind(format!(
                "{} ({detail})",
                rel.display()
            ))),
            EntryShape::File { size } => {
                if size > self.limits.max_file_size {
                    return Err(ExtractError::FileTooLarge(rel.display().to_string()));
                }
                self.counter.check_entry_count(&self.limits)?;
                self.counter.entries += 1;
                let dest = self.dest_root.join(rel);
                if let Some(parent) = dest.parent() {
                    self.ensure_dir(parent)?;
                }
                self.stream_to_file(reader, &dest)
            },
        }
    }

    fn ensure_dir(&mut self, path: &Path) -> Result<(), ExtractError> {
        if self.created_dirs.contains(path) {
            return Ok(());
        }
        ensure_dir(path)?;
        self.created_dirs.insert(path.to_path_buf());
        Ok(())
    }

    fn stream_to_file<R: Read>(&mut self, entry: &mut R, dest: &Path) -> Result<(), ExtractError> {
        let mut out = std::fs::File::create(dest)
            .map_err(|e| ExtractError::Io(format!("create {}: {e}", dest.display())))?;
        let mut written: u64 = 0;
        loop {
            let n = match entry.read(&mut self.buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => return Err(ExtractError::Io(e.to_string())),
            };
            let bytes = u64::try_from(n).map_err(|_| ExtractError::TotalTooLarge)?;
            let candidate = written
                .checked_add(bytes)
                .ok_or_else(|| ExtractError::FileTooLarge(dest.display().to_string()))?;
            if candidate > self.limits.max_file_size {
                return Err(ExtractError::FileTooLarge(dest.display().to_string()));
            }
            self.counter.add_bytes(bytes, &self.limits)?;
            out.write_all(&self.buf[..n])
                .map_err(|e| ExtractError::Io(format!("write {}: {e}", dest.display())))?;
            written = candidate;
        }
        Ok(())
    }

    fn check_deadline(&self) -> Result<(), ExtractError> {
        if Instant::now() > self.deadline {
            return Err(ExtractError::TimedOut);
        }
        Ok(())
    }
}

enum EntryShape {
    Dir,
    File { size: u64 },
    Link,
    Other(String),
}

/// Validate an archive entry's path. Returns:
/// - `Ok(Some(rel))` for entries to extract under `<dest>/<rel>`.
/// - `Ok(None)` for entries to skip (the strip-prefix wrapper itself,
///   or an entry whose path lies outside the prefix).
/// - `Err(...)` for paths that fail safety/limit checks.
fn sanitize_path(
    raw: &Path,
    strip_prefix: Option<&str>,
    limits: &ExtractorLimits,
) -> Result<Option<PathBuf>, ExtractError> {
    let stripped = match strip_prefix {
        None | Some("") => raw.to_path_buf(),
        Some(prefix) => {
            let prefix_path = Path::new(prefix.trim_end_matches('/'));
            let Ok(rest) = raw.strip_prefix(prefix_path) else {
                return Ok(None);
            };
            if rest.as_os_str().is_empty() {
                return Ok(None);
            }
            rest.to_path_buf()
        },
    };

    if stripped.as_os_str().is_empty() {
        return Ok(None);
    }
    if stripped.is_absolute() {
        return Err(ExtractError::UnsafePath(stripped.display().to_string()));
    }
    let mut depth: u32 = 0;
    for component in stripped.components() {
        use std::path::Component;
        match component {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {},
            _ => {
                return Err(ExtractError::UnsafePath(stripped.display().to_string()));
            },
        }
    }
    if depth > limits.max_path_depth {
        return Err(ExtractError::PathTooDeep(stripped.display().to_string()));
    }
    let path_len = u32::try_from(stripped.as_os_str().len()).unwrap_or(u32::MAX);
    if path_len > limits.max_path_len {
        return Err(ExtractError::PathTooLong(stripped.display().to_string()));
    }
    Ok(Some(stripped))
}

fn ensure_dir(path: &Path) -> Result<(), ExtractError> {
    if let Err(e) = std::fs::create_dir_all(path)
        && e.kind() != std::io::ErrorKind::AlreadyExists
    {
        return Err(ExtractError::Io(format!(
            "create_dir_all {}: {e}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;

    const CARGO_TOML_BYTES: &[u8] = b"[package]\nname = \"pkg\"\nversion = \"1.0.0\"\n";
    const LIB_RS_BYTES: &[u8] = b"pub fn answer() -> u32 { 42 }\n";

    fn synthesize_targz() -> Vec<u8> {
        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        {
            let mut tar = tar::Builder::new(&mut gz);
            append_targz_file(&mut tar, "pkg-1.0/Cargo.toml", CARGO_TOML_BYTES);
            append_targz_file(&mut tar, "pkg-1.0/src/lib.rs", LIB_RS_BYTES);
            tar.finish().unwrap();
        }
        gz.finish().unwrap()
    }

    pub(crate) fn append_targz_file(
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

    fn run_extract(
        bytes: &[u8],
        format: ArchiveFormat,
        strip: Option<&str>,
        limits: ExtractorLimits,
    ) -> (tempfile::TempDir, Result<ExtractStats, ExtractError>) {
        let tmp = tempfile::tempdir().unwrap();
        let blob_path = tmp.path().join("blob");
        std::fs::write(&blob_path, bytes).unwrap();
        let dest = tmp.path().join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        let result = extract(format, &blob_path, &dest, strip, limits);
        (tmp, result)
    }

    #[test]
    fn roundtrip_targz_extraction_writes_files_under_dest() {
        let bytes = synthesize_targz();
        let (tmp, result) = run_extract(
            &bytes,
            ArchiveFormat::TarGz,
            Some("pkg-1.0/"),
            DEFAULT_LIMITS,
        );
        let stats = result.expect("extract ok");
        assert_eq!(stats.entries, 2);
        assert_eq!(
            stats.bytes_written,
            (CARGO_TOML_BYTES.len() + LIB_RS_BYTES.len()) as u64
        );
        let dest = tmp.path().join("dest");
        let cargo = std::fs::read_to_string(dest.join("Cargo.toml")).unwrap();
        assert!(cargo.contains("name = \"pkg\""));
        let lib = std::fs::read_to_string(dest.join("src").join("lib.rs")).unwrap();
        assert!(lib.contains("answer"));
    }

    #[test]
    fn archive_limit_enforcement() {
        let bytes = synthesize_targz();

        let (_tmp, result) = run_extract(
            &bytes,
            ArchiveFormat::TarGz,
            Some("pkg-1.0/"),
            ExtractorLimits {
                max_entries: 1,
                ..DEFAULT_LIMITS
            },
        );
        assert!(matches!(result, Err(ExtractError::TooManyEntries)));

        let (_tmp, result) = run_extract(
            &bytes,
            ArchiveFormat::TarGz,
            Some("pkg-1.0/"),
            ExtractorLimits {
                max_file_size: 4,
                ..DEFAULT_LIMITS
            },
        );
        assert!(matches!(result, Err(ExtractError::FileTooLarge(_))));

        let (_tmp, result) = run_extract(
            &bytes,
            ArchiveFormat::TarGz,
            Some("pkg-1.0/"),
            ExtractorLimits {
                max_total_bytes: 30,
                ..DEFAULT_LIMITS
            },
        );
        assert!(matches!(
            result,
            Err(ExtractError::TotalTooLarge | ExtractError::FileTooLarge(_))
        ));
    }

    /// Hand-craft a single 512-byte tar header carrying a `../`-prefixed
    /// path so the extractor's `sanitize_path` is the thing under test
    /// (not `tar::Builder::set_path`, which rejects traversal upfront).
    fn raw_tar_with_path(path: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut header = [0u8; 512];
        // name (100 bytes)
        header[..path.len().min(100)].copy_from_slice(&path[..path.len().min(100)]);
        // mode "0000644 \0"
        header[100..108].copy_from_slice(b"0000644\0");
        // uid, gid: zeros padded
        header[108..116].copy_from_slice(b"0000000\0");
        header[116..124].copy_from_slice(b"0000000\0");
        // size: octal, 11 chars + NUL
        let size_str = format!("{:011o}\0", payload.len());
        header[124..136].copy_from_slice(size_str.as_bytes());
        // mtime
        header[136..148].copy_from_slice(b"00000000000\0");
        // checksum field initially spaces
        header[148..156].fill(b' ');
        // typeflag: regular file '0'
        header[156] = b'0';
        // ustar magic
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        // checksum: sum of all bytes treating cksum field as 8 spaces
        let cksum: u32 = header.iter().map(|&b| u32::from(b)).sum();
        let cksum_str = format!("{cksum:06o}\0 ");
        header[148..156].copy_from_slice(cksum_str.as_bytes());

        let mut out = Vec::new();
        out.extend_from_slice(&header);
        out.extend_from_slice(payload);
        // pad payload to 512-byte block
        let pad = (512 - (payload.len() % 512)) % 512;
        out.extend(std::iter::repeat_n(0u8, pad));
        // two zero blocks signal end of archive
        out.extend(std::iter::repeat_n(0u8, 1024));
        out
    }

    #[test]
    fn malicious_path_traversal_is_rejected() {
        let archive = raw_tar_with_path(b"../escape.txt", b"pwned");
        let (tmp, result) = run_extract(&archive, ArchiveFormat::Tar, None, DEFAULT_LIMITS);
        match result {
            Err(ExtractError::UnsafePath(_) | ExtractError::Malformed(_)) => {},
            other => panic!("expected UnsafePath/Malformed, got {other:?}"),
        }
        // The sanitize check plus lexical containment keep the write inside
        // dest; nothing escapes to the parent.
        let dest = tmp.path().join("dest");
        assert!(!dest.parent().unwrap().join("escape.txt").exists());
    }
}
