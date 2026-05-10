//! WASI-sandboxed archive extractor.
//!
//! Loaded by the omnifs host as an embedded Wasmtime component. The
//! host preopens the source archive at `/blob/blob.dat` (read-only)
//! and the destination directory at `/out/` (read-write), then calls
//! [`Guest::extract`] with the requested format and limits. The
//! component walks the archive, validates each entry, and writes
//! sanitized output under `/out/`. Limit trips return a typed
//! [`exports::omnifs::tool_archive::extract::ExtractError`]; the
//! host translates that into its own `ArchiveError`.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

wit_bindgen::generate!({
    world: "extractor",
    path: "../../wit/extractor",
});

use exports::omnifs::tool_archive::extract::{
    ArchiveFormat, ExtractError, ExtractOptions, ExtractStats, Guest,
};

// These are virtual WASI paths. The host grants them by preopening a
// staged blob directory at `/blob` and an extraction directory at `/out`.
const BLOB_PATH: &str = "/blob/blob.dat";
const OUT_ROOT: &str = "/out";

struct ExtractorComponent;

impl Guest for ExtractorComponent {
    fn extract(options: ExtractOptions) -> Result<ExtractStats, ExtractError> {
        let blob = std::fs::File::open(BLOB_PATH).map_err(|e| ExtractError::Io(e.to_string()))?;
        let strip_prefix = options.strip_prefix.as_deref();
        let limits = Limits::from(&options);

        match options.format {
            ArchiveFormat::TarGz => {
                let decoder = flate2::read::GzDecoder::new(blob);
                extract_tar(decoder, &limits, strip_prefix)
            },
            ArchiveFormat::Tar => extract_tar(blob, &limits, strip_prefix),
            ArchiveFormat::Zip => extract_zip(blob, &limits, strip_prefix),
        }
    }
}

export!(ExtractorComponent);

// The `max_` prefix is load-bearing here: the same fields appear in
// the WIT `extract-options` record and the host's `ExtractorLimits`.
// Renaming would diverge three structs; leave them aligned.
#[allow(clippy::struct_field_names)]
#[derive(Clone, Copy)]
struct Limits {
    max_entries: u64,
    max_file_size: u64,
    max_total_bytes: u64,
    max_path_depth: u32,
    max_path_len: u32,
}

impl From<&ExtractOptions> for Limits {
    fn from(options: &ExtractOptions) -> Self {
        Self {
            max_entries: options.max_entries,
            max_file_size: options.max_file_size,
            max_total_bytes: options.max_total_bytes,
            max_path_depth: options.max_path_depth,
            max_path_len: options.max_path_len,
        }
    }
}

#[derive(Default)]
struct Counter {
    entries: u64,
    bytes_written: u64,
}

impl Counter {
    fn check_entry_count(&self, limits: &Limits) -> Result<(), ExtractError> {
        (self.entries < limits.max_entries)
            .then_some(())
            .ok_or(ExtractError::TooManyEntries)
    }

    fn add_bytes(&mut self, n: u64, limits: &Limits) -> Result<(), ExtractError> {
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
    limits: Limits,
    counter: Counter,
    buf: Vec<u8>,
    created_dirs: HashSet<PathBuf>,
    strip_prefix: Option<&'a str>,
}

impl<'a> Extraction<'a> {
    fn new(strip_prefix: Option<&'a str>, limits: Limits) -> Self {
        Self {
            limits,
            counter: Counter::default(),
            buf: vec![0u8; 64 * 1024],
            created_dirs: HashSet::new(),
            strip_prefix,
        }
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
                let dest = join_out(rel);
                self.ensure_dir(&dest)
            },
            EntryShape::Link => {
                // Refuse links: a benign archive can express its layout
                // without them, and resolving them inside a sandbox is an
                // attack class we don't need to support.
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
                let dest = join_out(rel);
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
}

enum EntryShape {
    Dir,
    File { size: u64 },
    Link,
    Other(String),
}

fn extract_tar<R: Read>(
    reader: R,
    limits: &Limits,
    strip_prefix: Option<&str>,
) -> Result<ExtractStats, ExtractError> {
    let mut archive = tar::Archive::new(reader);
    let mut extraction = Extraction::new(strip_prefix, *limits);

    let entries = archive
        .entries()
        .map_err(|e| ExtractError::Malformed(e.to_string()))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| ExtractError::Malformed(e.to_string()))?;
        let raw_path = entry
            .path()
            .map_err(|e| ExtractError::Malformed(e.to_string()))?
            .into_owned();
        let Some(rel) = sanitize_path(&raw_path, extraction.strip_prefix, &extraction.limits)?
        else {
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
        extraction.process_entry(&rel, shape, &mut entry)?;
    }

    Ok(extraction.counter.into_stats())
}

fn extract_zip<R: Read + std::io::Seek>(
    reader: R,
    limits: &Limits,
    strip_prefix: Option<&str>,
) -> Result<ExtractStats, ExtractError> {
    let mut archive =
        zip::ZipArchive::new(reader).map_err(|e| ExtractError::Malformed(e.to_string()))?;
    let mut extraction = Extraction::new(strip_prefix, *limits);

    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| ExtractError::Malformed(e.to_string()))?;
        let Some(enclosed) = entry.enclosed_name() else {
            return Err(ExtractError::UnsafePath(entry.name().to_string()));
        };
        let Some(rel) = sanitize_path(&enclosed, extraction.strip_prefix, &extraction.limits)?
        else {
            continue;
        };
        let shape = if entry.is_dir() {
            EntryShape::Dir
        } else if entry.is_symlink() {
            EntryShape::Link
        } else {
            EntryShape::File { size: entry.size() }
        };
        extraction.process_entry(&rel, shape, &mut entry)?;
    }

    Ok(extraction.counter.into_stats())
}

/// Validate an archive entry's path. Returns:
/// - `Ok(Some(rel))` for entries to extract under `/out/<rel>`.
/// - `Ok(None)` for entries to skip (the strip-prefix wrapper itself,
///   or an entry whose path lies outside the prefix).
/// - `Err(...)` for paths that fail safety/limit checks.
fn sanitize_path(
    raw: &Path,
    strip_prefix: Option<&str>,
    limits: &Limits,
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

fn join_out(rel: &Path) -> PathBuf {
    Path::new(OUT_ROOT).join(rel)
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
