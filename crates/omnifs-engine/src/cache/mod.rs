pub(crate) mod body;
pub(crate) mod identity;
pub(crate) mod memory;
pub mod mount;
pub(crate) mod projection;

pub(crate) use identity::ProjectionId;
pub use mount::*;

/// Create an owned directory path without following any existing symlink in
/// any component. Cache roots use this before opening databases or body
/// stores, so an attacker cannot redirect a later path operation through a
/// substituted parent.
pub(crate) fn ensure_directory(path: &std::path::Path) -> std::io::Result<()> {
    let mut current = std::path::PathBuf::new();
    for component in path.components() {
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {},
            Ok(_) => return Err(std::io::Error::other("path is not an owned directory")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&current)?;
            },
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Resolve existing parent components without retaining a symlinked path. An
/// existing symlink at the requested root is rejected; a platform alias such
/// as `/var` on macOS is resolved to its real directory before new children
/// are created and checked component by component.
pub(crate) fn canonical_directory(path: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
    use std::ffi::OsString;
    use std::path::Component;

    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut requested = std::path::PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {},
            Component::ParentDir => {
                requested.pop();
            },
            other => requested.push(other.as_os_str()),
        }
    }

    let mut existing = requested.clone();
    let mut missing = Vec::<OsString>::new();
    loop {
        match std::fs::symlink_metadata(&existing) {
            Ok(metadata) => {
                if missing.is_empty() && metadata.file_type().is_symlink() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "requested directory is a symlink",
                    ));
                }
                if !metadata.is_dir() && !metadata.file_type().is_symlink() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::NotADirectory,
                        "requested directory is not a directory",
                    ));
                }
                let mut canonical = std::fs::canonicalize(&existing)?;
                for component in missing.iter().rev() {
                    canonical.push(component);
                }
                return Ok(canonical);
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let Some(name) = existing.file_name() else {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "directory has no existing parent",
                    ));
                };
                missing.push(name.to_os_string());
                existing.pop();
            },
            Err(error) => return Err(error),
        }
    }
}
