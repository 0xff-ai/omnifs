//! Shared client-state scaffolding.
//!
//! `client_state.rs` hangs its transitional journal state
//! off the same CLI-private root, both need owner-only (0o700) directories
//! and owner-only (0o600) files, and both need a locked sidecar file and an
//! atomic write. This module owns those mechanical filesystem steps; each
//! caller keeps its own domain types, error surface (anyhow vs thiserror),
//! and lock semantics (blocking vs try) on top.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;

use omnifs_bootstrap::Profile;

const CLIENT_DIR: &str = "client";

/// The CLI-private root every client-owned state tree hangs off.
pub(crate) fn client_root() -> anyhow::Result<PathBuf> {
    Ok(Profile::resolve()?.root().join(CLIENT_DIR))
}

/// Create `path` if absent and restrict it to owner-only (0o700). The
/// restriction is a no-op on non-unix targets, since there is no mode bit to
/// set there.
pub(crate) fn ensure_private_dir(path: &Path) -> io::Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// Open (creating if absent) a private (0o600) sidecar file, without
/// acquiring a lock on it. Callers that need to distinguish "could not even
/// open the file" from "the file is locked by another process" call
/// `fs2::FileExt::lock_exclusive`/`try_lock_exclusive` on the result
/// themselves, with their own error wrapping for each step.
pub(crate) fn open_private_sidecar(path: &Path) -> io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

/// Write `bytes` to `path` atomically (rename-on-commit) and owner-only
/// (0o600).
pub(crate) fn write_private_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use atomic_write_file::OpenOptions as AtomicOpenOptions;
    use std::io::Write as _;
    let mut options = AtomicOpenOptions::new();
    #[cfg(unix)]
    {
        // `.preserve_mode()` comes from `atomic_write_file::unix::OpenOptionsExt`;
        // `.mode()` is `atomic_write_file::OpenOptions`'s own impl of the
        // standard library's `OpenOptionsExt`. Both traits are needed.
        use atomic_write_file::unix::OpenOptionsExt as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        options.preserve_mode(false).mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.commit()
}
