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

/// Rename a completed temporary directory into its published path.
///
/// The destination must not already exist. Callers that intentionally
/// tolerate stale destinations should remove them before the final
/// publish step so this helper remains a plain rename.
pub(crate) fn publish_dir_by_rename(tmp: &Path, dest: &Path) -> std::io::Result<()> {
    std::fs::rename(tmp, dest)
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

/// Remove stale temp publish paths from a cache root.
pub(crate) fn sweep_temp_publish_dirs(root: &Path) -> std::io::Result<()> {
    if !root.exists() {
        std::fs::create_dir_all(root)?;
        return Ok(());
    }
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        if is_temp_publish_name(&file_name) {
            remove_existing_path(&entry.path())?;
        }
    }
    Ok(())
}

/// True when `name` matches temp paths produced by [`temp_sibling_path`].
pub(crate) fn is_temp_publish_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix('.') else {
        return false;
    };
    let Some((real_name, suffix)) = rest.rsplit_once(".tmp.") else {
        return false;
    };
    if real_name.is_empty() {
        return false;
    }
    let mut parts = suffix.split('.');
    let Some(pid) = parts.next() else {
        return false;
    };
    let Some(nanos) = parts.next() else {
        return false;
    };
    parts.next().is_none() && pid.parse::<u32>().is_ok() && nanos.parse::<u128>().is_ok()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_publish_name_matches_only_owned_tmp_pattern() {
        assert!(is_temp_publish_name(".7-targz-deadbeef.tmp.123.456"));
        assert!(!is_temp_publish_name(".tool.tmp.state"));
        assert!(!is_temp_publish_name("7-targz-deadbeef.tmp.123.456"));
        assert!(!is_temp_publish_name(".7-targz-deadbeef.tmp.123"));
    }
}
