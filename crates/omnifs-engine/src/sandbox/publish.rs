//! Publication helpers for host-owned cache artifacts.
//!
//! These helpers guarantee atomic visibility on a single filesystem by
//! writing or materializing at a sibling temporary path and then
//! renaming it into place. They do not claim crash durability because
//! they do not fsync the file and parent directory.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Write bytes to a sibling temp file and rename it over `path`.
pub(crate) fn replace_file_via_temp_rename(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = temp_sibling_path(path);
    if let Err(e) = std::fs::write(&tmp, bytes) {
        remove_path_best_effort(&tmp);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        remove_path_best_effort(&tmp);
        return Err(e);
    }
    Ok(())
}

/// Return a unique sibling temp path for a later rename into `dest`.
pub(crate) fn temp_sibling_path(dest: &Path) -> PathBuf {
    let name = dest
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    dest.with_file_name(format!(".{name}.tmp.{}.{nanos}", std::process::id()))
}

/// Publish a completed directory into an absent destination by rename.
pub(crate) fn publish_dir_by_rename(source: &Path, destination: &Path) -> std::io::Result<()> {
    let source_meta = std::fs::symlink_metadata(source)?;
    if !source_meta.is_dir() || source_meta.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "publish source is not a regular directory",
        ));
    }
    if let Some(parent) = destination.parent() {
        let metadata = std::fs::symlink_metadata(parent)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "publish destination parent is not a regular directory",
            ));
        }
    }
    std::fs::rename(source, destination)
}

/// Remove either a file or directory at `path`.
pub(crate) fn remove_existing_path(path: &Path) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

/// Best-effort cleanup for paths that may not exist or may already be gone.
pub(crate) fn remove_path_best_effort(path: &Path) {
    let _ = remove_existing_path(path);
}
